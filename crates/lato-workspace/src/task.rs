// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator_state.rs
// License: Apache-2.0
// Lato changes: extracted a provider-neutral workspace lease boundary and deterministic memory allocator without filesystem effects

use crate::lock_key;
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

static NEXT_MEMORY_LEASE_ID: AtomicU64 = AtomicU64::new(1);

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
    pub resource_key: Option<PathBuf>,
    #[serde(skip)]
    provenance: Option<LeaseProvenance>,
}

impl WorkspaceLease {
    pub fn new(
        id: LeaseId,
        task_id: TaskId,
        mode: WorkspaceMode,
        root: PathBuf,
        resource_key: Option<PathBuf>,
    ) -> Self {
        Self {
            id,
            task_id,
            mode,
            root,
            resource_key,
            provenance: None,
        }
    }
}

#[derive(Clone)]
struct LeaseProvenance {
    issuer: Arc<AllocatorIssuer>,
    original: LeaseMetadata,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LeaseMetadata {
    id: LeaseId,
    task_id: TaskId,
    mode: WorkspaceMode,
    root: PathBuf,
    resource_key: Option<PathBuf>,
}

impl LeaseMetadata {
    fn matches(&self, lease: &WorkspaceLease) -> bool {
        self.id == lease.id
            && self.task_id == lease.task_id
            && self.mode == lease.mode
            && self.root == lease.root
            && self.resource_key == lease.resource_key
    }
}

impl std::fmt::Debug for LeaseProvenance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<opaque>")
    }
}

impl PartialEq for LeaseProvenance {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.issuer, &other.issuer) && self.original == other.original
    }
}

impl Eq for LeaseProvenance {}

struct AllocatorIssuer;

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
    issuer: Arc<AllocatorIssuer>,
    fail_next: AtomicBool,
    live: Mutex<HashMap<LeaseId, WorkspaceLease>>,
}

impl MemoryWorkspaceAllocator {
    pub fn new(base_root: impl AsRef<Path>) -> Self {
        Self {
            inner: Arc::new(MemoryWorkspaceAllocatorInner {
                base_root: base_root.as_ref().to_path_buf(),
                issuer: Arc::new(AllocatorIssuer),
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
        let sequence = NEXT_MEMORY_LEASE_ID
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

    fn shared_resource_key(&self, root: &Path) -> Result<PathBuf, TaskError> {
        let absolute = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| {
                    TaskError::new(
                        TaskErrorCode::WorkspaceAllocation,
                        format!("failed to resolve current workspace directory: {error}"),
                    )
                })?
                .join(root)
        };
        let canonical = std::fs::canonicalize(&absolute).unwrap_or(absolute);
        Ok(lock_key(canonical))
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
        let resource_key = if mode == WorkspaceMode::SharedSerializedWrite {
            Some(self.shared_resource_key(&root)?)
        } else {
            None
        };
        let original = LeaseMetadata {
            id: id.clone(),
            task_id: request.task_id.clone(),
            mode,
            root: root.clone(),
            resource_key: resource_key.clone(),
        };
        let lease = WorkspaceLease {
            id: id.clone(),
            task_id: request.task_id,
            mode,
            root,
            resource_key,
            provenance: Some(LeaseProvenance {
                issuer: Arc::clone(&self.inner.issuer),
                original,
            }),
        };

        self.inner.live.lock().await.insert(id, lease.clone());
        Ok(lease)
    }

    async fn release(&self, lease: &WorkspaceLease) -> Result<(), TaskError> {
        let Some(provenance) = lease.provenance.as_ref() else {
            return Err(TaskError::new(
                TaskErrorCode::WorkspaceRelease,
                "workspace lease was not issued by this allocator",
            ));
        };
        if !Arc::ptr_eq(&provenance.issuer, &self.inner.issuer) {
            return Err(TaskError::new(
                TaskErrorCode::WorkspaceRelease,
                "workspace lease was not issued by this allocator",
            ));
        }
        if !provenance.original.matches(lease) {
            return Err(TaskError::new(
                TaskErrorCode::WorkspaceRelease,
                "workspace lease metadata differs from the issued lease",
            ));
        }

        let mut live = self.inner.live.lock().await;
        if live
            .get(&lease.id)
            .is_some_and(|registered| registered != lease)
        {
            return Err(TaskError::new(
                TaskErrorCode::WorkspaceRelease,
                "workspace lease does not match the registered lease",
            ));
        }
        live.remove(&lease.id);
        Ok(())
    }
}
