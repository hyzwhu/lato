use lato_core::{TaskErrorCode, TaskId, WorkspaceIntent};
use lato_workspace::{MemoryWorkspaceAllocator, WorkspaceAllocator, WorkspaceRequest};

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
