//! Inert workflow implementation: listing only, never executes.

use async_trait::async_trait;
use serde_json::Value;

use crate::{Workflow, WorkflowContext, WorkflowDescriptor, WorkflowError, WorkflowOutcome};

pub struct InertWorkflow {
    descriptor: WorkflowDescriptor,
}

impl InertWorkflow {
    pub fn new(descriptor: WorkflowDescriptor) -> Self {
        Self { descriptor }
    }
}

#[async_trait]
impl Workflow for InertWorkflow {
    fn descriptor(&self) -> &WorkflowDescriptor {
        &self.descriptor
    }

    async fn run(
        &self,
        _context: WorkflowContext,
        _input: Value,
    ) -> Result<WorkflowOutcome, WorkflowError> {
        let _ = &_input;
        Err(WorkflowError::NotImplemented)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use lato_core::SessionId;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{DEFAULT_AGENT_BUDGET, WorkflowContext};

    #[tokio::test]
    async fn run_is_not_implemented() {
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
                serde_json::json!({"x": 1}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "workflow.not_implemented");
    }
}
