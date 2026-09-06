use lato_core::{TaskErrorCode, TaskId, WorkspaceIntent};
use lato_workspace::{
    MemoryWorkspaceAllocator, WorkspaceAllocator, WorkspaceLease, WorkspaceMode, WorkspaceRequest,
};
use std::{collections::HashSet, path::PathBuf};

#[tokio::test]
async fn isolated_write_tasks_receive_distinct_leases() {
    let allocator = MemoryWorkspaceAllocator::new("/workspace");
    let first = allocator
        .allocate(WorkspaceRequest::new(
            TaskId::from("a"),
            WorkspaceIntent::IsolatedWorktree,
        ))
        .await
        .unwrap();
    let second = allocator
        .allocate(WorkspaceRequest::new(
            TaskId::from("b"),
            WorkspaceIntent::IsolatedWorktree,
        ))
        .await
        .unwrap();

    assert_ne!(first.id, second.id);
    assert_ne!(first.root, second.root);
    assert_eq!(allocator.live_count().await, 2);
}

#[tokio::test]
async fn release_is_idempotent() {
    let allocator = MemoryWorkspaceAllocator::new("/workspace");
    let lease = allocator
        .allocate(WorkspaceRequest::new(
            TaskId::from("a"),
            WorkspaceIntent::SharedReadOnly,
        ))
        .await
        .unwrap();

    allocator.release(&lease).await.unwrap();
    allocator.release(&lease).await.unwrap();

    assert_eq!(allocator.live_count().await, 0);
}

#[tokio::test]
async fn shared_read_only_tasks_use_the_same_root_without_a_resource_key() {
    let allocator = MemoryWorkspaceAllocator::new("/workspace");
    let first = allocator
        .allocate(WorkspaceRequest::new(
            TaskId::from("reader-a"),
            WorkspaceIntent::SharedReadOnly,
        ))
        .await
        .unwrap();
    let second = allocator
        .allocate(WorkspaceRequest::new(
            TaskId::from("reader-b"),
            WorkspaceIntent::SharedReadOnly,
        ))
        .await
        .unwrap();

    assert_eq!(first.root, second.root);
    assert_eq!(first.root, std::path::PathBuf::from("/workspace"));
    assert_eq!(first.resource_key, None);
    assert_eq!(second.resource_key, None);
}

#[tokio::test]
async fn shared_serialized_write_tasks_share_one_resource_key() {
    let allocator = MemoryWorkspaceAllocator::new("/workspace");
    let first = allocator
        .allocate(WorkspaceRequest::new(
            TaskId::from("writer-a"),
            WorkspaceIntent::SharedSerializedWrite,
        ))
        .await
        .unwrap();
    let second = allocator
        .allocate(WorkspaceRequest::new(
            TaskId::from("writer-b"),
            WorkspaceIntent::SharedSerializedWrite,
        ))
        .await
        .unwrap();

    assert_eq!(first.root, second.root);
    assert_eq!(first.resource_key, second.resource_key);
    assert!(first.resource_key.is_some());
}

#[tokio::test]
async fn relative_and_absolute_shared_roots_have_the_same_resource_key() {
    let relative_root = PathBuf::from("target/task-workspace-lock-key");
    let absolute_root = std::env::current_dir().unwrap().join(&relative_root);
    let relative = MemoryWorkspaceAllocator::new(&relative_root)
        .allocate(WorkspaceRequest::new(
            TaskId::from("relative"),
            WorkspaceIntent::SharedSerializedWrite,
        ))
        .await
        .unwrap();
    let absolute = MemoryWorkspaceAllocator::new(&absolute_root)
        .allocate(WorkspaceRequest::new(
            TaskId::from("absolute"),
            WorkspaceIntent::SharedSerializedWrite,
        ))
        .await
        .unwrap();

    assert_eq!(relative.resource_key, absolute.resource_key);
    assert_eq!(
        relative.resource_key,
        Some(lato_workspace::lock_key(absolute_root))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_shared_roots_have_the_same_resource_key() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &alias).unwrap();

    let direct = MemoryWorkspaceAllocator::new(&real)
        .allocate(WorkspaceRequest::new(
            TaskId::from("direct"),
            WorkspaceIntent::SharedSerializedWrite,
        ))
        .await
        .unwrap();
    let linked = MemoryWorkspaceAllocator::new(&alias)
        .allocate(WorkspaceRequest::new(
            TaskId::from("linked"),
            WorkspaceIntent::SharedSerializedWrite,
        ))
        .await
        .unwrap();

    assert_eq!(direct.resource_key, linked.resource_key);
    assert_eq!(direct.resource_key, Some(real.canonicalize().unwrap()));
}

#[tokio::test]
async fn allocation_failure_creates_no_live_lease() {
    let allocator = MemoryWorkspaceAllocator::new("/workspace");
    allocator.fail_next_allocation();

    let error = allocator
        .allocate(WorkspaceRequest::new(
            TaskId::from("failure"),
            WorkspaceIntent::ExternalLease,
        ))
        .await
        .unwrap_err();

    assert_eq!(error.code, TaskErrorCode::WorkspaceAllocation);
    assert_eq!(allocator.live_count().await, 0);
}

#[tokio::test]
async fn foreign_allocator_cannot_release_a_live_lease() {
    let owner = MemoryWorkspaceAllocator::new("/workspace");
    let foreign = MemoryWorkspaceAllocator::new("/workspace");
    let lease = owner
        .allocate(WorkspaceRequest::new(
            TaskId::from("owned"),
            WorkspaceIntent::SharedReadOnly,
        ))
        .await
        .unwrap();

    let error = foreign.release(&lease).await.unwrap_err();

    assert_eq!(error.code, TaskErrorCode::WorkspaceRelease);
    assert_eq!(owner.live_count().await, 1);
    assert_eq!(foreign.live_count().await, 0);
}

#[tokio::test]
async fn deserialized_or_forged_lease_cannot_release_a_live_lease() {
    let allocator = MemoryWorkspaceAllocator::new("/workspace");
    let authentic = allocator
        .allocate(WorkspaceRequest::new(
            TaskId::from("owned"),
            WorkspaceIntent::SharedReadOnly,
        ))
        .await
        .unwrap();
    let value = serde_json::to_value(&authentic).unwrap();
    let deserialized: WorkspaceLease = serde_json::from_value(value.clone()).unwrap();
    let mut forged_value = value;
    forged_value["task_id"] = serde_json::json!("forged");
    let forged: WorkspaceLease = serde_json::from_value(forged_value).unwrap();

    for invalid in [&deserialized, &forged] {
        let error = allocator.release(invalid).await.unwrap_err();
        assert_eq!(error.code, TaskErrorCode::WorkspaceRelease);
        assert_eq!(allocator.live_count().await, 1);
    }

    allocator.release(&authentic).await.unwrap();
    allocator.release(&authentic).await.unwrap();
    assert_eq!(allocator.live_count().await, 0);
}

#[tokio::test]
async fn authentic_provenance_cannot_authorize_tampered_lease_fields() {
    let allocator = MemoryWorkspaceAllocator::new("/workspace");
    let authentic = allocator
        .allocate(WorkspaceRequest::new(
            TaskId::from("owned"),
            WorkspaceIntent::SharedReadOnly,
        ))
        .await
        .unwrap();
    let mut tampered = authentic.clone();
    tampered.task_id = TaskId::from("different-task");

    let error = allocator.release(&tampered).await.unwrap_err();

    assert_eq!(error.code, TaskErrorCode::WorkspaceRelease);
    assert_eq!(allocator.live_count().await, 1);
    allocator.release(&authentic).await.unwrap();
}

#[tokio::test]
async fn lease_ids_are_unique_across_allocators_and_concurrent_allocations() {
    let first_allocator = MemoryWorkspaceAllocator::new("/first");
    let second_allocator = MemoryWorkspaceAllocator::new("/second");
    let mut joins = tokio::task::JoinSet::new();

    for index in 0..64 {
        let allocator = if index % 2 == 0 {
            first_allocator.clone()
        } else {
            second_allocator.clone()
        };
        joins.spawn(async move {
            allocator
                .allocate(WorkspaceRequest::new(
                    TaskId::from(format!("task-{index}")),
                    WorkspaceIntent::IsolatedWorktree,
                ))
                .await
                .unwrap()
                .id
        });
    }

    let mut ids = HashSet::new();
    while let Some(result) = joins.join_next().await {
        assert!(ids.insert(result.unwrap()));
    }
    assert_eq!(ids.len(), 64);
}

#[test]
fn workspace_modes_use_stable_snake_case_serde_names() {
    let cases = [
        (WorkspaceMode::SharedReadOnly, "shared_read_only"),
        (
            WorkspaceMode::SharedSerializedWrite,
            "shared_serialized_write",
        ),
        (WorkspaceMode::IsolatedWorktree, "isolated_worktree"),
        (WorkspaceMode::ExternalLease, "external_lease"),
    ];

    for (mode, expected) in cases {
        assert_eq!(serde_json::to_value(mode).unwrap(), expected);
        assert_eq!(
            serde_json::from_value::<WorkspaceMode>(serde_json::json!(expected)).unwrap(),
            mode
        );
    }
}

#[tokio::test]
async fn lease_serde_round_trip_preserves_visible_fields_only() {
    let allocator = MemoryWorkspaceAllocator::new("/workspace");
    let authentic = allocator
        .allocate(WorkspaceRequest::new(
            TaskId::from("serde"),
            WorkspaceIntent::SharedSerializedWrite,
        ))
        .await
        .unwrap();
    let value = serde_json::to_value(&authentic).unwrap();

    assert_eq!(value["id"], authentic.id.as_str());
    assert_eq!(value["task_id"], authentic.task_id.as_str());
    assert_eq!(value["mode"], "shared_serialized_write");
    assert_eq!(value["root"], "/workspace");
    assert_eq!(value["resource_key"], "/workspace");
    assert_eq!(value.as_object().unwrap().len(), 5);

    let restored: WorkspaceLease = serde_json::from_value(value).unwrap();
    assert_eq!(restored.id, authentic.id);
    assert_eq!(restored.task_id, authentic.task_id);
    assert_eq!(restored.mode, authentic.mode);
    assert_eq!(restored.root, authentic.root);
    assert_eq!(restored.resource_key, authentic.resource_key);
    assert_eq!(
        allocator.release(&restored).await.unwrap_err().code,
        TaskErrorCode::WorkspaceRelease
    );
    assert_eq!(allocator.live_count().await, 1);
}
