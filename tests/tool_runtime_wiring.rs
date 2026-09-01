#[test]
fn actor_uses_the_tool_runtime_for_definitions_and_execution() {
    let actor = include_str!("../crates/lato-agent/src/actor.rs");
    assert!(
        !actor.contains("dispatch("),
        "actor must not dispatch tools directly"
    );
    assert!(
        !actor.contains("v1_tool_definitions("),
        "actor must not own a second tool list"
    );
    assert!(actor.contains("tool_runtime.model_definitions()"));
    assert!(actor.contains("tool_runtime.invoke("));
}
