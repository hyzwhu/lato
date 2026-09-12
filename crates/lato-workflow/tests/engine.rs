use std::{future::ready, path::PathBuf, sync::Arc};

use futures_util::future::BoxFuture;
use lato_core::{
    AgentProfile, BudgetAccount, BudgetLimits, SessionId, TaskError, TaskProgress, TaskResult,
};
use lato_runtime::{
    ActiveMessageAdmission, CoordinatorConfig, NoopTaskEventSink, StartedTask, TaskChildControl,
    TaskCompletion, TaskReporter, TaskRunOutput, TaskRunRequest, TaskRunner,
    spawn_task_coordinator,
};
use lato_workflow::{
    DEFAULT_AGENT_BUDGET, WorkflowDescriptor, WorkflowDescriptorSet, WorkflowEngine, WorkflowStatus,
};
use lato_workspace::MemoryWorkspaceAllocator;

struct InstantControl;

impl TaskChildControl for InstantControl {
    fn progress(&self) -> TaskProgress {
        TaskProgress::default()
    }

    fn send_active_message(
        &self,
        delivery: lato_runtime::ActiveMessageDelivery,
    ) -> BoxFuture<'static, ActiveMessageAdmission> {
        Box::pin(ready(if delivery.commit_admission(|| ()).is_some() {
            ActiveMessageAdmission::Admitted
        } else {
            ActiveMessageAdmission::Rejected
        }))
    }

    fn cancel(&self) {}
}

struct InstantRunner;

#[async_trait::async_trait]
impl TaskRunner for InstantRunner {
    type Control = InstantControl;

    async fn run(
        &self,
        request: TaskRunRequest,
        reporter: TaskReporter<Self::Control>,
    ) -> TaskRunOutput {
        let _ = reporter
            .started(StartedTask::new(
                Arc::new(InstantControl),
                request.cancellation.clone(),
            ))
            .await;
        TaskRunOutput::from(TaskResult {
            success: true,
            output: "ok".into(),
            error: None,
            usage: Default::default(),
            duration_ms: 0,
            output_ref: None,
        })
    }

    async fn validate_profile(&self, _profile: &AgentProfile) -> Result<(), TaskError> {
        Ok(())
    }

    fn on_completed(&self, _completion: TaskCompletion) {}
}

struct HangRunner;

#[async_trait::async_trait]
impl TaskRunner for HangRunner {
    type Control = InstantControl;

    async fn run(
        &self,
        request: TaskRunRequest,
        reporter: TaskReporter<Self::Control>,
    ) -> TaskRunOutput {
        if reporter
            .started(StartedTask::new(
                Arc::new(InstantControl),
                request.cancellation.clone(),
            ))
            .await
        {
            request.cancellation.cancelled().await;
        } else {
            std::future::pending::<()>().await;
        }
        TaskRunOutput::from(TaskResult {
            success: false,
            output: String::new(),
            error: Some(TaskError::new(
                lato_core::TaskErrorCode::Cancelled,
                "cancelled",
            )),
            usage: Default::default(),
            duration_ms: 0,
            output_ref: None,
        })
    }

    async fn validate_profile(&self, _profile: &AgentProfile) -> Result<(), TaskError> {
        Ok(())
    }

    fn on_completed(&self, _completion: TaskCompletion) {}
}

fn descriptor(id: &str) -> WorkflowDescriptor {
    let (plugin, name) = id.split_once('/').unwrap();
    WorkflowDescriptor {
        id: id.into(),
        plugin_name: plugin.into(),
        name: name.into(),
        description: String::new(),
        when_to_use: String::new(),
        agent_budget: DEFAULT_AGENT_BUDGET,
        source_dir: PathBuf::from("."),
        generation: 1,
    }
}

fn set(ids: &[&str]) -> Arc<WorkflowDescriptorSet> {
    Arc::new(WorkflowDescriptorSet {
        generation: 1,
        workflows: ids
            .iter()
            .copied()
            .map(descriptor)
            .collect::<Vec<_>>()
            .into(),
        diagnostics: Arc::from([]),
    })
}

fn budget_one_child() -> BudgetAccount {
    let mut limits = BudgetLimits::unlimited();
    limits.child_tasks = Some(1);
    BudgetAccount::new(limits)
}

#[tokio::test]
async fn unknown_workflow_is_not_found() {
    let workspace = tempfile::tempdir().unwrap();
    let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig::default(),
        Arc::new(InstantRunner),
        allocator,
        Arc::new(NoopTaskEventSink),
    );
    let engine = WorkflowEngine::new(set(&[]), handle, budget_one_child());
    let err = engine
        .run(
            "demo/review",
            SessionId::from("sess"),
            serde_json::json!({}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "workflow.not_found");
    assert_eq!(engine.budget_reserved().child_tasks, 0);
    assert_eq!(engine.budget_spent().child_tasks, 0);
}

#[tokio::test]
async fn successful_run_settles_child_task_budget() {
    let workspace = tempfile::tempdir().unwrap();
    let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig::default(),
        Arc::new(InstantRunner),
        allocator,
        Arc::new(NoopTaskEventSink),
    );
    let engine = WorkflowEngine::new(set(&["demo/review"]), handle, budget_one_child());
    let outcome = engine
        .run(
            "demo/review",
            SessionId::from("sess"),
            serde_json::json!({"n": 1}),
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, WorkflowStatus::Completed);
    assert_eq!(outcome.output["workflow"], "demo/review");
    assert_eq!(engine.budget_reserved().child_tasks, 0);
    assert_eq!(engine.budget_spent().child_tasks, 1);
}

#[tokio::test]
async fn second_start_is_budget_denied_while_first_is_live() {
    let workspace = tempfile::tempdir().unwrap();
    let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig::default(),
        Arc::new(HangRunner),
        allocator,
        Arc::new(NoopTaskEventSink),
    );
    let engine = Arc::new(WorkflowEngine::new(
        set(&["demo/review"]),
        handle,
        budget_one_child(),
    ));
    let run_id = engine
        .start(
            "demo/review",
            SessionId::from("sess"),
            serde_json::json!({}),
        )
        .await
        .unwrap();
    let err = engine
        .start(
            "demo/review",
            SessionId::from("sess"),
            serde_json::json!({}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "workflow.budget_exceeded");
    engine.cancel(&run_id).await.unwrap();
}

#[tokio::test]
async fn cancel_unblocks_wait_and_releases_reservation() {
    let workspace = tempfile::tempdir().unwrap();
    let allocator = Arc::new(MemoryWorkspaceAllocator::new(workspace.path()).unwrap());
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig::default(),
        Arc::new(HangRunner),
        allocator,
        Arc::new(NoopTaskEventSink),
    );
    let engine = Arc::new(WorkflowEngine::new(
        set(&["demo/review"]),
        handle,
        budget_one_child(),
    ));
    let run_id = engine
        .start(
            "demo/review",
            SessionId::from("sess"),
            serde_json::json!({}),
        )
        .await
        .unwrap();
    let waiting = {
        let engine = Arc::clone(&engine);
        let run_id = run_id.clone();
        tokio::spawn(async move { engine.wait(&run_id).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    engine.cancel(&run_id).await.unwrap();
    let err = waiting.await.unwrap().unwrap_err();
    assert_eq!(err.code(), "workflow.cancelled");
    assert_eq!(engine.budget_reserved().child_tasks, 0);
    assert_eq!(engine.budget_spent().child_tasks, 0);
}
