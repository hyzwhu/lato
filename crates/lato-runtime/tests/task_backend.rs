use lato_runtime::{ChannelBackend, SubagentBackend, SubagentBackendResource};

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
