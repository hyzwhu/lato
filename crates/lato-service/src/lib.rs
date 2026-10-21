//! Authenticated local HTTP integration for application-owned Lato agents.

mod approval;
mod error;
mod events;
mod routes;
mod state;

use std::{future::IntoFuture, io, path::PathBuf, sync::Arc, time::Duration};

use lato_agent::{AcpHost, AgentConfiguration};
use lato_ai::ModelStream;
use lato_core::Tool;
use lato_workspace::{SandboxProfile, SessionTrust};
use tokio::{net::TcpListener, sync::mpsc};
use tokio_util::sync::CancellationToken;

use approval::ApprovalBridge;
use events::EventHub;
use state::ServiceState;

/// A service instance belongs to one trusted application/security principal.
/// Give separate applications their own Lato homes and service credentials.
pub struct ServiceConfig {
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub token: String,
    pub trust: SessionTrust,
    pub plugin_dirs: Vec<PathBuf>,
    pub agent: Option<AgentConfiguration>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub approval_timeout: Duration,
}

impl ServiceConfig {
    pub fn new(cwd: PathBuf, home: PathBuf, token: String) -> Self {
        let mut trust = SessionTrust::for_interactive(&cwd, false);
        trust.sandbox = SandboxProfile::Workspace;
        Self {
            cwd,
            home,
            token,
            trust,
            plugin_dirs: Vec::new(),
            agent: None,
            tools: Vec::new(),
            approval_timeout: Duration::from_secs(120),
        }
    }
}

pub struct AgentService {
    state: Arc<ServiceState>,
    relay: tokio::task::JoinHandle<()>,
}

impl AgentService {
    pub fn new(config: ServiceConfig, stream: Arc<dyn ModelStream>) -> Result<Self, String> {
        if config.token.len() < 32
            || config.token.len() > 1024
            || !config
                .token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
        {
            return Err("service.invalid_token: LATO_SERVICE_TOKEN must contain 32..1024 bearer-token characters".into());
        }
        if config.approval_timeout.is_zero() || config.approval_timeout > Duration::from_secs(300) {
            return Err(
                "service.invalid_timeout: approval timeout must be between zero and 300 seconds"
                    .into(),
            );
        }
        let events = Arc::new(EventHub::default());
        let approvals = ApprovalBridge::new(events.clone(), config.approval_timeout);
        let (tx, mut rx) = mpsc::unbounded_channel::<serde_json::Value>();
        let mut host = AcpHost::new_with_approval_home_and_plugin_dirs(
            config.cwd,
            config.trust,
            tx.clone(),
            stream,
            Some(approvals.clone()),
            config.home,
            config.plugin_dirs,
        );
        if let Some(configuration) = config.agent {
            host.configure_agent(configuration)?;
        }
        host.agent_configuration()?;
        for tool in config.tools {
            host.register_business_tool(tool)?;
        }
        let state = Arc::new(ServiceState::new(
            host,
            events.clone(),
            approvals,
            config.token,
            tx,
        ));
        let stop = state.stop.clone();
        let relay_state = Arc::downgrade(&state);
        let relay = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    value = rx.recv() => {
                        let Some(value) = value else {break};
                        if value["method"] == "_lato_service/flush" {
                            if let Some(state) = relay_state.upgrade()
                                && let Some(id) = value["id"].as_str()
                                && let Some(waiter) = state.relay_waiters.lock().unwrap_or_else(|p|p.into_inner()).remove(id)
                            { let _ = waiter.send(()); }
                            continue;
                        }
                        if let Some(id) = value.pointer("/params/sessionId").and_then(|v|v.as_str()) {
                            events.push(id,value["method"].as_str().unwrap_or("session/update"),value.clone());
                        }
                    }
                }
            }
        });
        Ok(Self { state, relay })
    }

    /// Serves a loopback listener until cancellation, then drains sessions.
    /// A dropped HTTP client never owns or cancels an admitted agent operation.
    pub async fn serve(self, listener: TcpListener, shutdown: CancellationToken) -> io::Result<()> {
        if !listener.local_addr()?.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "service must bind to a loopback address",
            ));
        }
        let serving = axum::serve(listener, routes::router(self.state.clone()))
            .with_graceful_shutdown(self.state.stop.clone().cancelled_owned())
            .into_future();
        tokio::pin!(serving);
        let result = tokio::select! {
            result = &mut serving => result,
            _ = shutdown.cancelled() => {
                self.state.begin_shutdown();
                tokio::time::timeout(Duration::from_secs(5),&mut serving).await.unwrap_or(Ok(()))
            }
        };
        self.state.shutdown().await;
        result
    }
}

impl Drop for AgentService {
    fn drop(&mut self) {
        self.state.begin_shutdown();
        self.relay.abort();
    }
}
