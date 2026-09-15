// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/worktree.rs
// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/worktree_cleanup.rs
// License: Apache-2.0
// Lato changes: transactional task leases under a configured repository-local root

use crate::{WorkspaceAllocator, WorkspaceLease, WorkspaceMode, WorkspaceRequest};
use lato_core::{LeaseId, TaskError, TaskErrorCode, WorkspaceIntent};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command, sync::Mutex};

pub const WORKTREE_OWNER_MARKER: &str = ".lato-task-owner.json";

static NEXT_GIT_LEASE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorktreeOwnerMarker {
    task_id: String,
    pid: u32,
    repository: PathBuf,
    created_unix_ms: u64,
}

#[derive(Clone, Debug)]
struct GitLeaseRecord {
    lease: WorkspaceLease,
}

#[derive(Clone)]
pub struct GitWorkspaceAllocator {
    inner: Arc<GitWorkspaceAllocatorInner>,
}

struct GitWorkspaceAllocatorInner {
    repository: PathBuf,
    worktrees_root: PathBuf,
    operation_lock: Mutex<()>,
    live: Mutex<HashMap<LeaseId, GitLeaseRecord>>,
}

impl GitWorkspaceAllocator {
    pub fn new(
        repository: impl AsRef<Path>,
        worktrees_root: impl AsRef<Path>,
    ) -> Result<Self, TaskError> {
        let configured_repository = repository.as_ref();
        let repository = normalize_path(std::fs::canonicalize(configured_repository).map_err(
            |error| {
                workspace_error(
                    TaskErrorCode::WorkspaceAllocation,
                    format!("failed to resolve Git repository: {error}"),
                )
            },
        )?);
        let git_dir = std::process::Command::new("git")
            .current_dir(&repository)
            .args(["rev-parse", "--git-dir"])
            .output()
            .map_err(|error| {
                workspace_error(
                    TaskErrorCode::WorkspaceAllocation,
                    format!("failed to invoke git: {error}"),
                )
            })?;
        if !git_dir.status.success() {
            return Err(workspace_error(
                TaskErrorCode::WorkspaceAllocation,
                "workspace root is not a Git repository",
            ));
        }

        let configured = worktrees_root.as_ref();
        let worktrees_root = normalize_path(if configured.is_absolute() {
            if let Ok(relative) = configured.strip_prefix(configured_repository) {
                repository.join(relative)
            } else {
                configured.to_path_buf()
            }
        } else {
            repository.join(configured)
        });
        if !worktrees_root.starts_with(&repository) {
            return Err(workspace_error(
                TaskErrorCode::WorkspaceAllocation,
                "task worktree root must be inside the source repository",
            ));
        }

        Ok(Self {
            inner: Arc::new(GitWorkspaceAllocatorInner {
                repository,
                worktrees_root,
                operation_lock: Mutex::new(()),
                live: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub async fn live_count(&self) -> usize {
        self.inner.live.lock().await.len()
    }

    pub async fn recover_stale(&self) -> Result<usize, TaskError> {
        let _guard = self.inner.operation_lock.lock().await;
        let mut entries = match tokio::fs::read_dir(&self.inner.worktrees_root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(workspace_error(
                    TaskErrorCode::WorkspaceRelease,
                    format!("failed to scan task worktrees: {error}"),
                ));
            }
        };
        let mut recovered = 0usize;
        while let Some(entry) = entries.next_entry().await.map_err(|error| {
            workspace_error(
                TaskErrorCode::WorkspaceRelease,
                format!("failed to read task worktree entry: {error}"),
            )
        })? {
            let path = entry.path();
            if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let marker_path = path.join(WORKTREE_OWNER_MARKER);
            let Ok(bytes) = tokio::fs::read(&marker_path).await else {
                continue;
            };
            let Ok(marker) = serde_json::from_slice::<WorktreeOwnerMarker>(&bytes) else {
                continue;
            };
            if marker.repository != self.inner.repository
                || crate::process::process_is_alive(marker.pid)
            {
                continue;
            }
            self.remove_worktree(&path).await?;
            recovered = recovered.saturating_add(1);
        }
        self.prune().await?;
        Ok(recovered)
    }

    async fn run_git(&self, args: &[&str]) -> Result<(), TaskError> {
        let output = Command::new("git")
            .current_dir(&self.inner.repository)
            .args(args)
            .output()
            .await
            .map_err(|error| {
                workspace_error(
                    TaskErrorCode::WorkspaceAllocation,
                    format!("failed to invoke git: {error}"),
                )
            })?;
        if output.status.success() {
            return Ok(());
        }
        Err(workspace_error(
            TaskErrorCode::WorkspaceAllocation,
            format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ))
    }

    async fn remove_worktree(&self, path: &Path) -> Result<(), TaskError> {
        let path = path.to_str().ok_or_else(|| {
            workspace_error(
                TaskErrorCode::WorkspaceRelease,
                "worktree path is not valid UTF-8",
            )
        })?;
        let output = Command::new("git")
            .current_dir(&self.inner.repository)
            .args(["worktree", "remove", "--force", path])
            .output()
            .await
            .map_err(|error| {
                workspace_error(
                    TaskErrorCode::WorkspaceRelease,
                    format!("failed to invoke git worktree remove: {error}"),
                )
            })?;
        if output.status.success() || !Path::new(path).exists() {
            return Ok(());
        }
        Err(workspace_error(
            TaskErrorCode::WorkspaceRelease,
            format!(
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ))
    }

    async fn prune(&self) -> Result<(), TaskError> {
        let output = Command::new("git")
            .current_dir(&self.inner.repository)
            .args(["worktree", "prune"])
            .output()
            .await
            .map_err(|error| {
                workspace_error(
                    TaskErrorCode::WorkspaceRelease,
                    format!("failed to invoke git worktree prune: {error}"),
                )
            })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(workspace_error(
                TaskErrorCode::WorkspaceRelease,
                format!(
                    "git worktree prune failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ))
        }
    }

    fn next_identity(&self, task_id: &str) -> Result<(LeaseId, String, PathBuf), TaskError> {
        let slug = sanitize_task_id(task_id)?;
        let sequence = NEXT_GIT_LEASE.fetch_add(1, Ordering::Relaxed);
        let suffix = format!("{}-{sequence}", std::process::id());
        let lease_id = LeaseId::from(format!("git-worktree-{slug}-{suffix}"));
        let branch = format!("lato/task-{slug}-{suffix}");
        let path = self.inner.worktrees_root.join(format!("{slug}-{suffix}"));
        Ok((lease_id, branch, path))
    }
}

#[async_trait::async_trait]
impl WorkspaceAllocator for GitWorkspaceAllocator {
    async fn allocate(&self, request: WorkspaceRequest) -> Result<WorkspaceLease, TaskError> {
        let _guard = self.inner.operation_lock.lock().await;
        let mode = WorkspaceMode::from(request.intent);
        if mode != WorkspaceMode::IsolatedWorktree {
            if request.intent != WorkspaceIntent::SharedReadOnly {
                return Err(workspace_error(
                    TaskErrorCode::WorkspaceAllocation,
                    "Git task allocator supports only shared read-only and isolated worktree leases",
                ));
            }
            let sequence = NEXT_GIT_LEASE.fetch_add(1, Ordering::Relaxed);
            let id = LeaseId::from(format!("git-shared-{}-{sequence}", std::process::id()));
            let lease = WorkspaceLease::new(
                id.clone(),
                request.task_id,
                mode,
                self.inner.repository.clone(),
                None,
            );
            self.inner.live.lock().await.insert(
                id,
                GitLeaseRecord {
                    lease: lease.clone(),
                },
            );
            return Ok(lease);
        }

        let (id, branch, path) = self.next_identity(request.task_id.as_str())?;
        tokio::fs::create_dir_all(&self.inner.worktrees_root)
            .await
            .map_err(|error| {
                workspace_error(
                    TaskErrorCode::WorkspaceAllocation,
                    format!("failed to create task worktree root: {error}"),
                )
            })?;
        let path_text = path.to_str().ok_or_else(|| {
            workspace_error(
                TaskErrorCode::WorkspaceAllocation,
                "worktree path is not valid UTF-8",
            )
        })?;
        self.run_git(&["worktree", "add", "-b", &branch, path_text, "HEAD"])
            .await?;

        let marker = WorktreeOwnerMarker {
            task_id: request.task_id.as_str().to_owned(),
            pid: std::process::id(),
            repository: self.inner.repository.clone(),
            created_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        };
        let marker_bytes = serde_json::to_vec(&marker).map_err(|error| {
            workspace_error(
                TaskErrorCode::WorkspaceAllocation,
                format!("failed to encode worktree owner marker: {error}"),
            )
        })?;
        if let Err(error) = tokio::fs::write(path.join(WORKTREE_OWNER_MARKER), marker_bytes).await {
            let _ = self.remove_worktree(&path).await;
            let _ = self.prune().await;
            return Err(workspace_error(
                TaskErrorCode::WorkspaceAllocation,
                format!("failed to write worktree owner marker: {error}"),
            ));
        }

        let lease = WorkspaceLease::new(id.clone(), request.task_id, mode, path, None);
        self.inner.live.lock().await.insert(
            id,
            GitLeaseRecord {
                lease: lease.clone(),
            },
        );
        Ok(lease)
    }

    async fn release(&self, lease: &WorkspaceLease) -> Result<(), TaskError> {
        let _guard = self.inner.operation_lock.lock().await;
        let record = self.inner.live.lock().await.get(&lease.id).cloned();
        let Some(record) = record else {
            return Ok(());
        };
        if record.lease.task_id != lease.task_id
            || record.lease.root != lease.root
            || record.lease.mode != lease.mode
        {
            return Err(workspace_error(
                TaskErrorCode::WorkspaceRelease,
                "workspace lease metadata does not match the issued lease",
            ));
        }
        if lease.mode == WorkspaceMode::IsolatedWorktree {
            self.remove_worktree(&lease.root).await?;
            self.prune().await?;
        }
        self.inner.live.lock().await.remove(&lease.id);
        Ok(())
    }
}

fn sanitize_task_id(task_id: &str) -> Result<String, TaskError> {
    let slug: String = task_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() || slug == "." || slug == ".." {
        return Err(workspace_error(
            TaskErrorCode::InvalidIdentity,
            "task ID cannot be converted to a safe worktree name",
        ));
    }
    Ok(slug.chars().take(80).collect())
}

fn workspace_error(code: TaskErrorCode, message: impl Into<String>) -> TaskError {
    TaskError::new(code, message)
}

/// Strips the verbatim prefix that `std::fs::canonicalize` produces on Windows
/// (`\\?\C:\...`, `\\?\UNC\server\share`). Git rejects verbatim paths when
/// creating worktrees ("could not create leading directories ... Invalid
/// argument"), so paths handed to git or used as worktree roots must be
/// plain DOS paths.
fn normalize_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.as_os_str().to_string_lossy();
        if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(plain) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(plain);
        }
        path
    }
    #[cfg(not(windows))]
    path
}
