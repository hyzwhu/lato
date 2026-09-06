// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator_state.rs
// License: Apache-2.0
// Lato changes: extracted a provider-neutral workspace lease boundary and deterministic memory allocator without filesystem effects

use lato_core::{LeaseId, TaskError, TaskErrorCode, TaskId, WorkspaceIntent};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::Mutex;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    SharedReadOnly,
    SharedSerializedWrite,
    IsolatedWorktree,
    ExternalLease,
}

impl From<WorkspaceIntent> for WorkspaceMode {
    fn from(intent: WorkspaceIntent) -> Self {
        match intent {
            WorkspaceIntent::SharedReadOnly => Self::SharedReadOnly,
            WorkspaceIntent::SharedSerializedWrite => Self::SharedSerializedWrite,
            WorkspaceIntent::IsolatedWorktree => Self::IsolatedWorktree,
            WorkspaceIntent::ExternalLease => Self::ExternalLease,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct WorkspaceLease {
    pub id: LeaseId,
    pub task_id: TaskId,
    pub mode: WorkspaceMode,
    pub root: PathBuf,
    pub resource_key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct WorkspaceRequest {
    pub task_id: TaskId,
    pub intent: WorkspaceIntent,
}

impl WorkspaceRequest {
    pub fn new(task_id: TaskId, intent: WorkspaceIntent) -> Self {
        Self { task_id, intent }
    }
}

#[async_trait::async_trait]
pub trait WorkspaceAllocator: Send + Sync + 'static {
    async fn allocate(&self, request: WorkspaceRequest) -> Result<WorkspaceLease, TaskError>;
    async fn release(&self, lease: &WorkspaceLease) -> Result<(), TaskError>;
}

#[derive(Clone)]
pub struct MemoryWorkspaceAllocator {
    inner: Arc<MemoryWorkspaceAllocatorInner>,
}

struct MemoryWorkspaceAllocatorInner {
    base_root: PathBuf,
    next_id: AtomicU64,
    fail_next: AtomicBool,
    live: Mutex<HashMap<LeaseId, WorkspaceLease>>,
}

impl MemoryWorkspaceAllocator {
    pub fn new(base_root: impl AsRef<Path>) -> Self {
        Self {
            inner: Arc::new(MemoryWorkspaceAllocatorInner {
                base_root: base_root.as_ref().to_path_buf(),
                next_id: AtomicU64::new(1),
                fail_next: AtomicBool::new(false),
                live: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn fail_next_allocation(&self) {
        self.inner.fail_next.store(true, Ordering::Release);
    }

    pub async fn live_count(&self) -> usize {
        self.inner.live.lock().await.len()
    }

    fn next_lease_id(&self) -> Result<LeaseId, TaskError> {
        let sequence = self
            .inner
            .next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| {
                TaskError::new(
                    TaskErrorCode::WorkspaceAllocation,
                    "workspace lease identifier space is exhausted",
                )
            })?;
        Ok(LeaseId::from(format!("workspace-lease-{sequence}")))
    }

    fn lease_root(&self, mode: WorkspaceMode, lease_id: &LeaseId) -> PathBuf {
        match mode {
            WorkspaceMode::SharedReadOnly | WorkspaceMode::SharedSerializedWrite => {
                self.inner.base_root.clone()
            }
            WorkspaceMode::IsolatedWorktree => self
                .inner
                .base_root
                .join(".lato-task-workspaces")
                .join(lease_id.as_str()),
            WorkspaceMode::ExternalLease => self
                .inner
                .base_root
                .join(".lato-external-leases")
                .join(lease_id.as_str()),
        }
    }
}

#[async_trait::async_trait]
impl WorkspaceAllocator for MemoryWorkspaceAllocator {
    async fn allocate(&self, request: WorkspaceRequest) -> Result<WorkspaceLease, TaskError> {
        if self.inner.fail_next.swap(false, Ordering::AcqRel) {
            return Err(TaskError::new(
                TaskErrorCode::WorkspaceAllocation,
                "injected memory workspace allocation failure",
            ));
        }

        let id = self.next_lease_id()?;
        let mode = WorkspaceMode::from(request.intent);
        let root = self.lease_root(mode, &id);
        let resource_key = (mode == WorkspaceMode::SharedSerializedWrite)
            .then(|| format!("shared-workspace:{}", root.display()));
        let lease = WorkspaceLease {
            id: id.clone(),
            task_id: request.task_id,
            mode,
            root,
            resource_key,
        };

        self.inner.live.lock().await.insert(id, lease.clone());
        Ok(lease)
    }

    async fn release(&self, lease: &WorkspaceLease) -> Result<(), TaskError> {
        self.inner.live.lock().await.remove(&lease.id);
        Ok(())
    }
}
