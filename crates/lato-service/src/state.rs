use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::http::StatusCode;
use futures_util::{FutureExt, StreamExt};
use lato_agent::{AcpHost, RuntimePromptOutcome, RuntimeSession};
use lato_protocol::JsonRpcReq;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::{approval::ApprovalBridge, error::ApiError, events::EventHub};

pub(crate) const MAX_SESSIONS: usize = 64;
pub(crate) const MAX_OPERATIONS: usize = 128;
pub(crate) const MAX_ACTIVE_OPERATIONS: usize = 16;
const MAX_RESULT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SkillRequest {
    pub name: String,
    pub args: Option<String>,
    pub context: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperationRequest {
    pub text: Option<String>,
    pub skill: Option<SkillRequest>,
}

impl OperationRequest {
    pub fn validate(&self) -> Result<(), ApiError> {
        let valid = match (&self.text, &self.skill) {
            (Some(text), None) => !text.trim().is_empty() && text.len() <= 64 * 1024,
            (None, Some(skill)) => {
                !skill.name.trim().is_empty()
                    && skill.name.len() <= 256
                    && skill.args.as_ref().is_none_or(|a| a.len() <= 8 * 1024)
                    && skill.context.as_ref().is_none_or(|c| c.len() <= 32 * 1024)
            }
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "service.invalid_operation",
                "provide either non-empty text (at most 64 KiB) or one bounded skill invocation",
            ))
        }
    }

    fn title_input(&self) -> String {
        match &self.skill {
            Some(skill) => format!(
                "/skill {} {}",
                skill.name,
                skill.args.as_deref().unwrap_or_default()
            ),
            None => self.text.clone().unwrap_or_default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationStatus {
    Accepted,
    Running,
    Cancelling,
    Completed,
    Cancelled,
    Failed,
}

impl OperationStatus {
    fn active(self) -> bool {
        matches!(self, Self::Accepted | Self::Running | Self::Cancelling)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OperationView {
    pub api_version: &'static str,
    pub operation_id: String,
    pub session_id: String,
    pub status: OperationStatus,
    pub result: Option<Value>,
    pub error: Option<Value>,
}

struct Operation {
    view: OperationView,
    cancel: CancellationToken,
    abort: Option<tokio::task::AbortHandle>,
}

pub(crate) struct SessionSlot {
    pub runtime: Arc<RuntimeSession>,
    pub operation: Option<String>,
    pub closing: bool,
}

#[derive(Default)]
pub(crate) struct Registry {
    pub sessions: BTreeMap<String, SessionSlot>,
    operations: BTreeMap<String, Operation>,
    order: VecDeque<String>,
}

pub(crate) struct ServiceState {
    pub host: AsyncMutex<AcpHost>,
    pub lifecycle: AsyncMutex<()>,
    pub registry: Mutex<Registry>,
    pub events: Arc<EventHub>,
    pub approvals: Arc<ApprovalBridge>,
    pub token: String,
    pub stop: CancellationToken,
    pub stopping: AtomicBool,
    pub relay_sender: tokio::sync::mpsc::UnboundedSender<Value>,
    pub relay_waiters: Mutex<BTreeMap<String, tokio::sync::oneshot::Sender<()>>>,
    prefix: u128,
    next_id: AtomicU64,
}

impl ServiceState {
    pub fn new(
        host: AcpHost,
        events: Arc<EventHub>,
        approvals: Arc<ApprovalBridge>,
        token: String,
        relay_sender: tokio::sync::mpsc::UnboundedSender<Value>,
    ) -> Self {
        Self {
            host: AsyncMutex::new(host),
            lifecycle: AsyncMutex::new(()),
            registry: Mutex::new(Registry::default()),
            events,
            approvals,
            token,
            stop: CancellationToken::new(),
            stopping: AtomicBool::new(false),
            relay_sender,
            relay_waiters: Mutex::new(BTreeMap::new()),
            prefix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn require_running(&self) -> Result<(), ApiError> {
        if self.stopping.load(Ordering::Acquire) {
            Err(ApiError::unavailable())
        } else {
            Ok(())
        }
    }

    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        let mut host = self.host.lock().await;
        let future = host.handle(JsonRpcReq {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params: Some(params),
        });
        let response = future
            .await
            .ok_or_else(|| ApiError::runtime("agent returned no response"))?;
        if let Some(error) = response.get("error") {
            return Err(ApiError::runtime(
                error["message"]
                    .as_str()
                    .unwrap_or("agent operation failed"),
            ));
        }
        Ok(response["result"].clone())
    }

    pub fn session(&self, id: &str) -> Result<Arc<RuntimeSession>, ApiError> {
        self.registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sessions
            .get(id)
            .map(|s| s.runtime.clone())
            .ok_or_else(ApiError::missing)
    }

    pub fn operation(&self, id: &str) -> Result<OperationView, ApiError> {
        self.registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .operations
            .get(id)
            .map(|op| op.view.clone())
            .ok_or_else(ApiError::missing)
    }

    async fn flush_updates(&self, id: &str) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.relay_waiters
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.into(), tx);
        let _ = self
            .relay_sender
            .send(json!({"method":"_lato_service/flush","id":id}));
        let _ = tokio::time::timeout(Duration::from_secs(1), rx).await;
        self.relay_waiters
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
    }

    pub fn submit(
        self: &Arc<Self>,
        session_id: &str,
        request: OperationRequest,
    ) -> Result<OperationView, ApiError> {
        self.require_running()?;
        request.validate()?;
        let id = format!(
            "op-{:x}-{}",
            self.prefix,
            self.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let token = CancellationToken::new();
        let view = OperationView {
            api_version: "v1",
            operation_id: id.clone(),
            session_id: session_id.into(),
            status: OperationStatus::Accepted,
            result: None,
            error: None,
        };
        let runtime = {
            let mut registry = self.registry.lock().unwrap_or_else(|p| p.into_inner());
            self.require_running()?;
            let slot = registry
                .sessions
                .get(session_id)
                .ok_or_else(ApiError::missing)?;
            if slot.operation.is_some() || slot.closing {
                return Err(ApiError::busy());
            }
            let runtime = slot.runtime.clone();
            if registry
                .operations
                .values()
                .filter(|o| o.view.status.active())
                .count()
                >= MAX_ACTIVE_OPERATIONS
            {
                return Err(ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "service.capacity",
                    "too many active operations",
                ));
            }
            while registry.operations.len() >= MAX_OPERATIONS {
                let Some(index) = registry.order.iter().position(|id| {
                    registry
                        .operations
                        .get(id)
                        .is_some_and(|o| !o.view.status.active())
                }) else {
                    return Err(ApiError::new(
                        StatusCode::TOO_MANY_REQUESTS,
                        "service.capacity",
                        "operation history is full",
                    ));
                };
                if let Some(old) = registry.order.remove(index) {
                    registry.operations.remove(&old);
                }
            }
            registry
                .sessions
                .get_mut(session_id)
                .expect("slot exists")
                .operation = Some(id.clone());
            registry.order.push_back(id.clone());
            registry.operations.insert(
                id.clone(),
                Operation {
                    view: view.clone(),
                    cancel: token.clone(),
                    abort: None,
                },
            );
            runtime
        };
        self.events
            .push(session_id, "operation_accepted", json!(view));
        let guard = OperationGuard {
            state: self.clone(),
            id: id.clone(),
            done: false,
        };
        let task = tokio::spawn(run_operation(guard, runtime, request, token));
        if let Some(operation) = self
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .operations
            .get_mut(&id)
        {
            operation.abort = Some(task.abort_handle());
        }
        Ok(view)
    }

    pub fn cancel(&self, session_id: &str) -> Result<Option<OperationView>, ApiError> {
        let view = {
            let mut registry = self.registry.lock().unwrap_or_else(|p| p.into_inner());
            let id = registry
                .sessions
                .get(session_id)
                .ok_or_else(ApiError::missing)?
                .operation
                .clone();
            id.and_then(|id| registry.operations.get_mut(&id))
                .map(|op| {
                    op.cancel.cancel();
                    op.view.status = OperationStatus::Cancelling;
                    op.view.clone()
                })
        };
        self.approvals.deny_session(session_id);
        if let Some(view) = &view {
            self.events
                .push(session_id, "operation_cancelling", json!(view));
        }
        Ok(view)
    }

    fn finish(
        &self,
        id: &str,
        status: OperationStatus,
        result: Option<Value>,
        error: Option<Value>,
    ) {
        let view = {
            let mut registry = self.registry.lock().unwrap_or_else(|p| p.into_inner());
            let Some(op) = registry.operations.get_mut(id) else {
                return;
            };
            if !op.view.status.active() {
                return;
            }
            op.view.status = status;
            op.view.result = result;
            op.view.error = error;
            op.abort = None;
            let view = op.view.clone();
            if let Some(slot) = registry.sessions.get_mut(&view.session_id)
                && slot.operation.as_deref() == Some(id)
            {
                slot.operation = None;
            }
            view
        };
        self.events
            .push(&view.session_id, "operation_finished", json!(view));
    }

    pub fn begin_shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        self.stop.cancel();
        self.approvals.deny_all();
        let registry = self.registry.lock().unwrap_or_else(|p| p.into_inner());
        for operation in registry
            .operations
            .values()
            .filter(|o| o.view.status.active())
        {
            operation.cancel.cancel();
        }
    }

    pub async fn shutdown(&self) {
        self.begin_shutdown();
        let sessions: Vec<_> = self
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sessions
            .iter()
            .map(|(id, s)| (id.clone(), s.runtime.clone()))
            .collect();
        // Runtime cancellation remains authoritative; force-aborting is only a final
        // shutdown fallback and leaves any uncertain side effect in the journal.
        let drain = async {
            let mut closing = futures_util::stream::FuturesUnordered::new();
            for (_, runtime) in &sessions {
                closing.push(runtime.cancel_and_join(Duration::from_secs(3)));
            }
            while closing.next().await.is_some() {}
            for (id, _) in &sessions {
                let _ = self.rpc("session/close", json!({"sessionId":id})).await;
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(8), drain).await;
        let aborts: Vec<_> = self
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .operations
            .values()
            .filter_map(|o| o.abort.clone())
            .collect();
        for abort in aborts {
            abort.abort();
        }
    }
}

struct OperationGuard {
    state: Arc<ServiceState>,
    id: String,
    done: bool,
}
impl Drop for OperationGuard {
    fn drop(&mut self) {
        if !self.done {
            self.state.finish(&self.id,OperationStatus::Failed,None,Some(json!({
            "code":"service.operation_interrupted","message":"operation interrupted; inspect the canonical journal before retrying"})));
        }
    }
}

async fn run_operation(
    mut guard: OperationGuard,
    runtime: Arc<RuntimeSession>,
    request: OperationRequest,
    cancel: CancellationToken,
) {
    if cancel.is_cancelled() {
        guard
            .state
            .finish(&guard.id, OperationStatus::Cancelled, None, None);
        guard.done = true;
        return;
    }
    {
        let mut registry = guard
            .state
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(op) = registry.operations.get_mut(&guard.id)
            && op.view.status == OperationStatus::Accepted
        {
            op.view.status = OperationStatus::Running;
        }
    }
    let input = request.title_input();
    let prompt = async {
        match request.skill {
            Some(skill) => {
                runtime
                    .prompt_skill_with_context(skill.name, skill.args, skill.context)
                    .await
            }
            None => runtime.prompt(request.text.unwrap_or_default()).await,
        }
    };
    let outcome = std::panic::AssertUnwindSafe(async {
        tokio::pin!(prompt);
        tokio::select! {
            biased;
            result = &mut prompt => result,
            _ = cancel.cancelled() => {
                // Keep polling the prompt while cancellation waits for its submission
                // gate. Dropping the prompt here would poison a reusable session.
                let cancellation = runtime.cancel();
                tokio::pin!(cancellation);
                tokio::select! {
                    result = &mut prompt => result,
                    _ = &mut cancellation => prompt.await,
                }
            }
        }
    })
    .catch_unwind()
    .await;
    guard.state.flush_updates(&guard.id).await;
    match outcome {
        Ok(Ok(RuntimePromptOutcome::Complete {mut text})) => {
            let original_bytes = text.len();
            if original_bytes > MAX_RESULT_BYTES {
                let mut end = MAX_RESULT_BYTES;
                while !text.is_char_boundary(end) { end -= 1; }
                text.truncate(end);
            }
            guard.state.finish(&guard.id,OperationStatus::Completed,Some(json!({"text":text,"truncated":original_bytes > MAX_RESULT_BYTES,"originalBytes":original_bytes})),None);
        },
        Ok(Ok(RuntimePromptOutcome::Cancelled {..})) => guard.state.finish(&guard.id,OperationStatus::Cancelled,None,None),
        Ok(Err(error)) => guard.state.finish(&guard.id,OperationStatus::Failed,None,Some(json!(error))),
        Err(_) => guard.state.finish(&guard.id,OperationStatus::Failed,None,Some(json!({"code":"service.operation_panicked","message":"agent operation failed internally"}))),
    }
    guard.done = true;
    guard
        .state
        .host
        .lock()
        .await
        .record_prompt_metadata(runtime.session_id().as_str(), &input)
        .await;
}
