use std::path::PathBuf;

use lato_core::{BudgetAccount, BudgetAmount, BudgetLimits, SessionId};
use lato_workflow::{
    DEFAULT_AGENT_BUDGET, InertWorkflow, Workflow, WorkflowContext, WorkflowDescriptor,
};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn inert_run_does_not_reserve_budget() {
    let account = BudgetAccount::new(BudgetLimits::unlimited());
    let spent = account.spent();
    let reserved = account.reserved();
    let workflow = InertWorkflow::new(WorkflowDescriptor {
        id: "demo/review".into(),
        plugin_name: "demo".into(),
        name: "review".into(),
        description: String::new(),
        when_to_use: String::new(),
        agent_budget: DEFAULT_AGENT_BUDGET,
        source_dir: PathBuf::from("."),
        generation: 1,
    });
    let err = workflow
        .run(
            WorkflowContext {
                run_id: "run-1".into(),
                session_id: SessionId::from("sess"),
                generation: 1,
                cancel: CancellationToken::new(),
                agent_budget: DEFAULT_AGENT_BUDGET,
            },
            serde_json::json!({}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "workflow.not_implemented");
    assert_eq!(account.spent(), spent);
    assert_eq!(account.reserved(), reserved);
    assert_eq!(account.spent(), BudgetAmount::ZERO);
    assert_eq!(account.open_reservations(), 0);
}
