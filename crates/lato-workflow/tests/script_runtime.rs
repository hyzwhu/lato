#[test]
fn validate_only_completes_without_live_host_side_effects() {
    let script = r#"
let meta = #{ name: "ok", description: "d" };
let r = agent("hello", #{ label: "a" });
complete(r.output);
"#;
    let report = lato_workflow::script::validate_script(script, None).unwrap();
    assert!(report.outcome_ok);
}

#[test]
fn empty_agent_prompt_is_rejected() {
    let script = r#"
let meta = #{ name: "bad", description: "d" };
agent("");
"#;
    assert!(lato_workflow::script::validate_script(script, None).is_err());
}
