use lato_service::{AgentService, ServiceConfig};
use lato_workspace::{ApprovalMode, SandboxProfile};
use tokio_util::sync::CancellationToken;

use crate::args::{SandboxArg, ServeArgs};

pub async fn run(args: ServeArgs) -> i32 {
    match start(args).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

async fn start(args: ServeArgs) -> Result<(), String> {
    let token = std::env::var("LATO_SERVICE_TOKEN").map_err(|_| {
        "service.invalid_token: set LATO_SERVICE_TOKEN before starting the service".to_owned()
    })?;
    // Reject service credentials before any provider refresh or listener binding.
    if token.len() < 32
        || token.len() > 1024
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
    {
        return Err("service.invalid_token: LATO_SERVICE_TOKEN must contain 32..1024 bearer-token characters".into());
    }
    let application = args
        .agent_config
        .as_deref()
        .map(lato_agent::AgentConfiguration::from_file)
        .transpose()?;
    let stream = crate::cli::configured_stream(&args.model).await?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut config = ServiceConfig::new(cwd, crate::cli::lato_home(), token);
    config.agent = application;
    if args.trust_workspace {
        config.trust = lato_workspace::SessionTrust::for_interactive(&config.cwd, true);
    }
    config.plugin_dirs = args.plugin_dirs;
    config.trust.sandbox = match args.sandbox {
        SandboxArg::Off => SandboxProfile::Off,
        SandboxArg::Workspace => SandboxProfile::Workspace,
        SandboxArg::ReadOnly => SandboxProfile::ReadOnly,
    };
    if args.auto_approve {
        config.trust.mode = ApprovalMode::Always;
    }
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .map_err(|e| format!("service.bind: {e}"))?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let service = AgentService::new(config, stream)?;
    let stop = CancellationToken::new();
    let signal_stop = stop.clone();
    let signal = tokio::spawn(async move {
        wait_for_shutdown().await;
        signal_stop.cancel();
    });
    eprintln!("Lato service listening on http://{address}; API v1; bearer authentication required");
    let result = service
        .serve(listener, stop)
        .await
        .map_err(|e| format!("service.serve: {e}"));
    signal.abort();
    result
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
