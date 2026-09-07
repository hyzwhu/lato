// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/backend.rs
// License: Apache-2.0
// Lato changes: adapts the backend resource to Lato's bounded Phase 5A coordinator handle

use super::{
    ActiveMessageOutcome, ActiveMessageRequest, CancelOutcome, ScopedTaskHandle, SpawnDisposition,
    SpawnTaskRequest, TaskSnapshot, WaitOutcome,
};
use async_trait::async_trait;
use lato_core::{TaskError, TaskErrorCode, TaskId};
use std::{sync::Arc, time::Duration};

/// Session-bound product membrane for subagent lifecycle operations.
///
/// Implementations must preserve coordinator ownership checks. A backend is
/// intentionally incapable of minting a new scope or naming a different root.
#[async_trait]
pub trait SubagentBackend: Send + Sync + 'static {
    async fn spawn(&self, request: SpawnTaskRequest) -> Result<SpawnDisposition, TaskError>;

    async fn send_active_message(&self, request: ActiveMessageRequest) -> ActiveMessageOutcome;

    async fn wait(&self, task_id: TaskId, timeout: Duration) -> Result<WaitOutcome, TaskError>;

    async fn cancel(&self, task_id: TaskId) -> Result<CancelOutcome, TaskError>;

    async fn inspect(&self, task_id: TaskId) -> Result<TaskSnapshot, TaskError>;

    async fn list_running(&self) -> Result<Vec<TaskSnapshot>, TaskError>;

    async fn teardown_root_and_drain(&self) -> Result<CancelOutcome, TaskError>;

    async fn validate_profile(&self, profile: &str) -> Result<(), TaskError>;
}

/// In-process backend matching Grok Build's channel-backed product boundary.
///
/// `ScopedTaskHandle` already owns Lato's bounded coordinator channels, so this
/// adapter does not introduce a second mailbox or actor.
#[derive(Clone)]
pub struct ChannelBackend {
    handle: ScopedTaskHandle,
}

impl ChannelBackend {
    pub fn new(handle: ScopedTaskHandle) -> Self {
        Self { handle }
    }

    pub fn scoped_handle(&self) -> &ScopedTaskHandle {
        &self.handle
    }

    pub fn is_builtin_profile(profile: &str) -> bool {
        matches!(profile, "explorer" | "worker" | "reviewer")
    }

    pub fn into_resource(self) -> SubagentBackendResource {
        SubagentBackendResource(Arc::new(self))
    }
}

#[async_trait]
impl SubagentBackend for ChannelBackend {
    async fn spawn(&self, request: SpawnTaskRequest) -> Result<SpawnDisposition, TaskError> {
        self.handle.spawn(request).await
    }

    async fn send_active_message(&self, request: ActiveMessageRequest) -> ActiveMessageOutcome {
        self.handle.send_active_message(request).await
    }

    async fn wait(&self, task_id: TaskId, timeout: Duration) -> Result<WaitOutcome, TaskError> {
        self.handle.wait(task_id, timeout).await
    }

    async fn cancel(&self, task_id: TaskId) -> Result<CancelOutcome, TaskError> {
        self.handle.cancel_task(task_id).await
    }

    async fn inspect(&self, task_id: TaskId) -> Result<TaskSnapshot, TaskError> {
        self.handle.inspect(task_id).await
    }

    async fn list_running(&self) -> Result<Vec<TaskSnapshot>, TaskError> {
        self.handle.list_running().await
    }

    async fn teardown_root_and_drain(&self) -> Result<CancelOutcome, TaskError> {
        self.handle.teardown_root_and_drain().await
    }

    async fn validate_profile(&self, profile: &str) -> Result<(), TaskError> {
        if Self::is_builtin_profile(profile) {
            return Ok(());
        }
        Err(TaskError::new(
            TaskErrorCode::InvalidProfile,
            format!("unknown built-in task profile: {profile}"),
        ))
    }
}

/// Resource injected into a parent or child session's tool runtime.
#[derive(Clone)]
pub struct SubagentBackendResource(pub Arc<dyn SubagentBackend>);

impl SubagentBackendResource {
    pub fn new(backend: Arc<dyn SubagentBackend>) -> Self {
        Self(backend)
    }

    pub fn backend(&self) -> &dyn SubagentBackend {
        self.0.as_ref()
    }
}

impl std::fmt::Debug for SubagentBackendResource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubagentBackendResource")
            .finish_non_exhaustive()
    }
}
