#[test]
fn actor_has_no_legacy_name_or_counter_approval_bypass() {
    let actor = include_str!("../crates/lato-agent/src/actor.rs");
    for forbidden in ["requires_approval", "has_allow_once", ".allow_once()"] {
        assert!(
            !actor.contains(forbidden),
            "actor bypass remains: {forbidden}"
        );
    }
}

#[test]
fn console_approval_does_not_render_raw_arguments() {
    let cli = include_str!("../src/cli.rs");
    let implementation = cli
        .split("impl ToolApproval for ConsoleToolApproval")
        .nth(1)
        .expect("console approval implementation");
    let implementation = implementation
        .split("pub async fn run")
        .next()
        .expect("console approval implementation boundary");
    assert!(implementation.contains("ApprovalRequest"));
    assert!(!implementation.contains("arguments"));
}
