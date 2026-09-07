use lato_runtime::{
    ChannelBackend, SubagentBackend, SubagentBackendResource, spawn_subagent_coordinator,
    spawn_task_coordinator,
};

fn assert_backend_object_safe(_: &dyn SubagentBackend) {}

#[test]
fn subagent_backend_is_object_safe_and_resource_is_cloneable() {
    fn assert_clone<T: Clone>() {}
    assert_clone::<ChannelBackend>();
    assert_clone::<SubagentBackendResource>();
    let _ = assert_backend_object_safe;
}

#[test]
fn builtin_profile_validation_is_closed() {
    assert!(ChannelBackend::is_builtin_profile("explorer"));
    assert!(ChannelBackend::is_builtin_profile("worker"));
    assert!(ChannelBackend::is_builtin_profile("reviewer"));
    assert!(!ChannelBackend::is_builtin_profile("custom"));
    assert!(!ChannelBackend::is_builtin_profile(""));
}

#[test]
fn compatibility_and_product_constructors_are_both_exported() {
    let _ = spawn_subagent_coordinator::<NeverRunner, lato_workspace::MemoryWorkspaceAllocator>;
    let _ = spawn_task_coordinator::<NeverRunner, lato_workspace::MemoryWorkspaceAllocator>;
}

struct NeverControl;

impl lato_runtime::TaskChildControl for NeverControl {
    fn progress(&self) -> lato_core::TaskProgress {
        lato_core::TaskProgress::default()
    }

    fn send_active_message(
        &self,
        _delivery: lato_runtime::ActiveMessageDelivery,
    ) -> futures_util::future::BoxFuture<'static, lato_runtime::ActiveMessageAdmission> {
        Box::pin(async { lato_runtime::ActiveMessageAdmission::Rejected })
    }

    fn cancel(&self) {}
}

struct NeverRunner;

#[async_trait::async_trait]
impl lato_runtime::TaskRunner for NeverRunner {
    type Control = NeverControl;

    async fn run(
        &self,
        _request: lato_runtime::TaskRunRequest,
        _reporter: lato_runtime::TaskReporter<Self::Control>,
    ) -> lato_runtime::TaskRunOutput {
        unreachable!()
    }

    async fn validate_profile(
        &self,
        _profile: &lato_core::AgentProfile,
    ) -> Result<(), lato_core::TaskError> {
        Ok(())
    }

    fn on_completed(&self, _completion: lato_runtime::TaskCompletion) {}
}
