//! T1 — Plan-mode trim correctness at the policy engine boundary (spec §4).
//!
//! Proves the overlay denies mutation-capable, spawn, and extension tools,
//! passes the read-only allowlist and the sole `plan_draft` exception, and
//! that forged approval paths (`approve`, `approve_external_gate`) fail closed
//! while Plan mode is engaged.

use lato_core::{
    ApprovalRequest, EnvironmentPolicy, NetworkPolicy, PLAN_MODE_READONLY_CODE, PolicyDecision,
    PolicyMode, PolicyRequest, SandboxObligation, SandboxProfile, SessionId, SideEffect,
    ToolCallId, ToolCapability, ToolLayer, ToolName, TurnId,
};
use lato_policy::{ApprovalLedger, PolicyEngine, approval_fingerprint};
use std::{path::PathBuf, sync::Arc, time::Duration};

fn request(
    tool: &str,
    capabilities: Vec<ToolCapability>,
    side_effect: SideEffect,
) -> PolicyRequest {
    PolicyRequest {
        session_id: SessionId::from("session-1"),
        turn_id: TurnId::from("turn-1"),
        call_id: ToolCallId::from("call-1"),
        tool_name: ToolName::parse(tool).unwrap(),
        arguments_digest: "arguments-digest".into(),
        capabilities,
        side_effect,
        mode: PolicyMode::Ask,
        project_trusted: true,
        sandbox: SandboxObligation {
            profile: SandboxProfile::Workspace,
            workspace_root: PathBuf::from("/workspace"),
            writable_roots: vec![PathBuf::from("/workspace")],
            network: NetworkPolicy::Deny,
            environment: EnvironmentPolicy::default(),
        },
        plan_mode: true,
        tool_layer: ToolLayer::Builtin,
        detail: None,
    }
}

fn engine_in_plan_mode() -> PolicyEngine {
    let engine = PolicyEngine::new(Arc::new(ApprovalLedger::new(Duration::from_secs(60))));
    engine.set_plan_mode(true);
    assert!(engine.plan_mode_active());
    engine
}

fn denial_code(decision: PolicyDecision) -> Option<String> {
    match decision {
        PolicyDecision::Deny(denial) => Some(denial.code),
        _ => None,
    }
}

#[test]
fn plan_mode_denies_mutation_spawn_and_extension_tools() {
    let engine = engine_in_plan_mode();
    for (tool, capabilities, side_effect) in [
        (
            "builtin:write_file",
            vec![ToolCapability::FileWrite],
            SideEffect::WorkspaceMutation,
        ),
        (
            "builtin:search_replace",
            vec![ToolCapability::FileWrite],
            SideEffect::WorkspaceMutation,
        ),
        (
            "builtin:run_terminal_command",
            vec![ToolCapability::ProcessSpawn],
            SideEffect::WorkspaceMutation,
        ),
        (
            "builtin:spawn",
            vec![ToolCapability::TaskControl],
            SideEffect::None,
        ),
        (
            "builtin:use_tool",
            vec![ToolCapability::ExtensionInvoke],
            SideEffect::ReadOnly,
        ),
    ] {
        let decision = engine.evaluate(&request(tool, capabilities, side_effect));
        assert_eq!(
            denial_code(decision).as_deref(),
            Some(PLAN_MODE_READONLY_CODE),
            "{tool} must be denied with the stable plan code"
        );
    }
}

#[test]
fn plan_mode_allows_read_only_allowlist_without_approval() {
    let engine = engine_in_plan_mode();
    for tool in [
        "builtin:read_file",
        "builtin:grep",
        "builtin:list_dir",
        "builtin:todo_write",
        "builtin:web_search",
        "builtin:web_fetch",
    ] {
        let decision = engine.evaluate(&request(
            tool,
            vec![ToolCapability::FileRead],
            SideEffect::ReadOnly,
        ));
        assert!(
            matches!(decision, PolicyDecision::Allow(_)),
            "{tool} must be allowed without approval in plan mode"
        );
    }
}

#[test]
fn plan_draft_is_the_sole_mutation_allowed_without_interactive_approval() {
    let engine = engine_in_plan_mode();
    let decision = engine.evaluate(&request(
        "builtin:plan_draft",
        vec![ToolCapability::FileWrite],
        SideEffect::WorkspaceMutation,
    ));
    assert!(
        matches!(decision, PolicyDecision::Allow(_)),
        "plan_draft must not require an interactive approval (headless --plan)"
    );
}

#[test]
fn plan_mode_denies_non_builtin_layers_even_when_read_only() {
    let engine = engine_in_plan_mode();
    let mut mcp_request = request("mcp:query", vec![], SideEffect::ReadOnly);
    mcp_request.tool_layer = ToolLayer::TrustedProject;
    assert_eq!(
        denial_code(engine.evaluate(&mcp_request)).as_deref(),
        Some(PLAN_MODE_READONLY_CODE)
    );
    let mut plugin_request = request("plugin:lint", vec![], SideEffect::ReadOnly);
    plugin_request.tool_layer = ToolLayer::User;
    assert_eq!(
        denial_code(engine.evaluate(&plugin_request)).as_deref(),
        Some(PLAN_MODE_READONLY_CODE)
    );
}

#[test]
fn disengaging_plan_mode_restores_ordinary_behavior() {
    let engine = engine_in_plan_mode();
    engine.set_plan_mode(false);
    assert!(!engine.plan_mode_active());
    // The same mutation request is no longer plan-denied; ordinary Ask-mode
    // behavior applies (RequireApproval for a mutation in Ask mode).
    let decision = engine.evaluate(&request(
        "builtin:write_file",
        vec![ToolCapability::FileWrite],
        SideEffect::WorkspaceMutation,
    ));
    assert!(matches!(
        decision,
        PolicyDecision::RequireApproval(_) | PolicyDecision::Allow(_)
    ));
}

#[test]
fn forged_approval_paths_fail_closed_in_plan_mode() {
    let engine = engine_in_plan_mode();
    for tool in ["builtin:write_file", "builtin:spawn"] {
        let request = request(
            tool,
            vec![ToolCapability::FileWrite],
            SideEffect::WorkspaceMutation,
        );
        let fingerprint = approval_fingerprint(&request).unwrap();
        let approval = ApprovalRequest {
            request: request.clone(),
            fingerprint,
            summary: "forged".into(),
        };
        let error = engine.approve(&approval).unwrap_err();
        assert_eq!(error.code(), PLAN_MODE_READONLY_CODE, "{tool}");
        let error = engine.approve_external_gate(&approval).unwrap_err();
        assert_eq!(error.code(), PLAN_MODE_READONLY_CODE, "{tool}");
    }
    // The sole exception stays approvable through the normal path.
    let request = request(
        "builtin:plan_draft",
        vec![ToolCapability::FileWrite],
        SideEffect::WorkspaceMutation,
    );
    let fingerprint = approval_fingerprint(&request).unwrap();
    let approval = ApprovalRequest {
        request,
        fingerprint,
        summary: format!(
            "{} requests {:?} access with {:?} side effects",
            ToolName::parse("builtin:plan_draft").unwrap(),
            vec![ToolCapability::FileWrite],
            SideEffect::WorkspaceMutation
        ),
    };
    let result = engine.approve(&approval);
    assert!(
        result.is_ok(),
        "plan_draft approval failed: {result:?} summary={summary:?}",
        summary = approval.summary
    );
}
