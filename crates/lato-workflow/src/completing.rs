//! Coordinator runner that completes a workflow step without calling a model.

use std::{future::ready, sync::Arc};

use futures_util::future::BoxFuture;
use lato_core::{AgentProfile, TaskError, TaskProgress, TaskResult};
use lato_runtime::{
    ActiveMessageAdmission, StartedTask, TaskChildControl, TaskCompletion, TaskReporter,
    TaskRunOutput, TaskRunRequest, TaskRunner,
};

pub struct CompletingControl;

impl TaskChildControl for CompletingControl {
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

/// Completes each spawned step immediately. Used by CLI until a model-backed
/// runner is wired in 7B3.
#[derive(Default)]
pub struct CompletingTaskRunner;

#[async_trait::async_trait]
impl TaskRunner for CompletingTaskRunner {
    type Control = CompletingControl;

    async fn run(
        &self,
        request: TaskRunRequest,
        reporter: TaskReporter<Self::Control>,
    ) -> TaskRunOutput {
        let _ = reporter
            .started(StartedTask::new(
                Arc::new(CompletingControl),
                request.cancellation.clone(),
            ))
            .await;
        TaskRunOutput::from(TaskResult {
            success: true,
            output: request.node.scope.objective,
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
