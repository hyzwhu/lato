use lato_core::{TaskId, WorkspaceIntent};
use lato_workspace::{GitWorkspaceAllocator, WorkspaceAllocator, WorkspaceRequest};
use std::{path::Path, process::Command};
use tempfile::TempDir;

struct Repo {
    temp: TempDir,
}

impl Repo {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(
            temp.path(),
            &["config", "user.email", "lato@example.invalid"],
        );
        git(temp.path(), &["config", "user.name", "Lato Test"]);
        std::fs::write(temp.path().join("README.md"), "root\n").unwrap();
        git(temp.path(), &["add", "README.md"]);
        git(temp.path(), &["commit", "-qm", "initial"]);
        Self { temp }
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }

    fn allocator(&self) -> GitWorkspaceAllocator {
        GitWorkspaceAllocator::new(self.root(), self.root().join(".lato/worktrees")).unwrap()
    }
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn worker(task_id: &str) -> WorkspaceRequest {
    WorkspaceRequest::new(TaskId::from(task_id), WorkspaceIntent::IsolatedWorktree)
}

#[tokio::test]
async fn parallel_worker_leases_are_git_isolated_and_release_is_idempotent() {
    let repo = Repo::new();
    let allocator = repo.allocator();
    let (left, right) = tokio::join!(
        allocator.allocate(worker("left")),
        allocator.allocate(worker("right"))
    );
    let left = left.unwrap();
    let right = right.unwrap();

    assert_ne!(left.root, right.root);
    std::fs::write(left.root.join("shared.txt"), "left").unwrap();
    assert!(!right.root.join("shared.txt").exists());
    assert!(!repo.root().join("shared.txt").exists());

    allocator.release(&left).await.unwrap();
    allocator.release(&left).await.unwrap();
    allocator.release(&right).await.unwrap();
    assert!(!left.root.exists());
    assert!(!right.root.exists());
    assert_eq!(allocator.live_count().await, 0);
}

#[tokio::test]
async fn read_only_lease_uses_shared_repository_without_creating_worktree() {
    let repo = Repo::new();
    let allocator = repo.allocator();
    let lease = allocator
        .allocate(WorkspaceRequest::new(
            TaskId::from("explorer"),
            WorkspaceIntent::SharedReadOnly,
        ))
        .await
        .unwrap();
    assert_eq!(lease.root, std::fs::canonicalize(repo.root()).unwrap());
    allocator.release(&lease).await.unwrap();
    assert_eq!(allocator.live_count().await, 0);
}

#[tokio::test]
async fn stale_dead_owner_worktree_is_recovered_but_live_owner_is_preserved() {
    let repo = Repo::new();
    let allocator = repo.allocator();
    let stale = allocator.allocate(worker("stale")).await.unwrap();
    let live = allocator.allocate(worker("live")).await.unwrap();

    let marker = stale.root.join(lato_workspace::WORKTREE_OWNER_MARKER);
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
    value["pid"] = serde_json::json!(4_000_000_000u64);
    std::fs::write(&marker, serde_json::to_vec(&value).unwrap()).unwrap();

    let recovered = repo.allocator().recover_stale().await.unwrap();
    assert_eq!(recovered, 1);
    assert!(!stale.root.exists());
    assert!(live.root.exists());
    allocator.release(&live).await.unwrap();
}
