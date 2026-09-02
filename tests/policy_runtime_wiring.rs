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
fn tui_approval_does_not_render_raw_arguments() {
    let backend = include_str!("../src/tui/backend.rs");
    let implementation = backend
        .split("impl ToolApproval for TuiToolApproval")
        .nth(1)
        .expect("TUI approval implementation");
    let implementation = implementation
        .split("pub struct BackendHandle")
        .next()
        .expect("TUI approval implementation boundary");
    assert!(implementation.contains("ApprovalRequest"));
    assert!(!implementation.contains("arguments"));
}
