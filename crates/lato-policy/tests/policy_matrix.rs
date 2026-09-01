use lato_core::{
    ApprovalFingerprint, EnvironmentPolicy, NetworkPolicy, PolicyDecision, PolicyMode,
    PolicyRequest, SandboxObligation, SandboxProfile, SessionId, SideEffect, ToolCallId,
    ToolCapability, ToolName, TurnId,
};
use lato_policy::{ApprovalLedger, PolicyEngine, PolicyError, approval_fingerprint};
use std::{path::PathBuf, sync::Arc, time::Duration};

fn request(
    mode: PolicyMode,
    capabilities: Vec<ToolCapability>,
    side_effect: SideEffect,
    project_trusted: bool,
) -> PolicyRequest {
    PolicyRequest {
        session_id: SessionId::from("session-1"),
        turn_id: TurnId::from("turn-1"),
        call_id: ToolCallId::from("call-1"),
        tool_name: ToolName::parse("builtin:test").unwrap(),
        arguments_digest: "arguments-digest".into(),
        capabilities,
        side_effect,
        mode,
        project_trusted,
        sandbox: SandboxObligation {
            profile: SandboxProfile::Workspace,
            workspace_root: PathBuf::from("/workspace"),
            writable_roots: vec![PathBuf::from("/workspace")],
            network: NetworkPolicy::Deny,
            environment: EnvironmentPolicy::default(),
        },
    }
}

fn engine() -> PolicyEngine {
    PolicyEngine::new(Arc::new(ApprovalLedger::new(Duration::from_secs(60))))
}

#[test]
fn ask_allows_read_but_requires_exact_approval_for_risky_calls() {
    let engine = engine();

    assert!(matches!(
        engine.evaluate(&request(
            PolicyMode::Ask,
            vec![ToolCapability::FileRead],
            SideEffect::ReadOnly,
            true,
        )),
        PolicyDecision::Allow(_)
    ));
    assert!(matches!(
        engine.evaluate(&request(
            PolicyMode::Ask,
            vec![ToolCapability::FileWrite],
            SideEffect::WorkspaceMutation,
            true,
        )),
        PolicyDecision::RequireApproval(_)
    ));
    assert!(matches!(
        engine.evaluate(&request(
            PolicyMode::Ask,
            vec![ToolCapability::ProcessSpawn],
            SideEffect::ReadOnly,
            true,
        )),
        PolicyDecision::RequireApproval(_)
    ));
    assert!(matches!(
        engine.evaluate(&request(
            PolicyMode::Ask,
            vec![ToolCapability::NetworkWrite],
            SideEffect::ExternalMutation,
            true,
        )),
        PolicyDecision::RequireApproval(_)
    ));
}

#[test]
fn always_and_auto_issue_internal_grants_for_mutation() {
    let engine = engine();

    for mode in [PolicyMode::Always, PolicyMode::Auto] {
        let request = request(
            mode,
            vec![ToolCapability::FileWrite],
            SideEffect::WorkspaceMutation,
            true,
        );
        let PolicyDecision::Allow(grant) = engine.evaluate(&request) else {
            panic!("{mode:?} must issue an automatic execution grant");
        };
        let expected = approval_fingerprint(&request).unwrap();
        assert_eq!(engine.consume(&grant, &expected), Ok(()));
        assert_eq!(
            engine.consume(&grant, &expected),
            Err(PolicyError::Approval(
                lato_policy::ApprovalError::ConsumedOrMissing
            ))
        );
    }
}

#[test]
fn untrusted_extension_is_denied_in_every_mode() {
    let engine = engine();

    for mode in [PolicyMode::Ask, PolicyMode::Always, PolicyMode::Auto] {
        let decision = engine.evaluate(&request(
            mode,
            vec![ToolCapability::ExtensionInvoke, ToolCapability::FileRead],
            SideEffect::ReadOnly,
            false,
        ));
        let PolicyDecision::Deny(denial) = decision else {
            panic!("untrusted extension must fail closed in {mode:?}");
        };
        assert_eq!(denial.code, "policy.untrusted_extension");
    }
}

#[test]
fn trusted_extension_uses_the_same_side_effect_matrix() {
    let engine = engine();
    assert!(matches!(
        engine.evaluate(&request(
            PolicyMode::Ask,
            vec![ToolCapability::ExtensionInvoke, ToolCapability::FileRead],
            SideEffect::ReadOnly,
            true,
        )),
        PolicyDecision::Allow(_)
    ));
    assert!(matches!(
        engine.evaluate(&request(
            PolicyMode::Ask,
            vec![ToolCapability::ExtensionInvoke, ToolCapability::FileWrite],
            SideEffect::WorkspaceMutation,
            true,
        )),
        PolicyDecision::RequireApproval(_)
    ));
}

#[test]
fn approve_recomputes_the_fingerprint_and_rejects_tampering() {
    let engine = engine();
    let original_request = request(
        PolicyMode::Ask,
        vec![ToolCapability::FileWrite],
        SideEffect::WorkspaceMutation,
        true,
    );
    let PolicyDecision::RequireApproval(approval) = engine.evaluate(&original_request) else {
        panic!("write should require approval");
    };

    let mut forged_digest = approval.clone();
    forged_digest.fingerprint = ApprovalFingerprint("caller-supplied".into());
    assert_eq!(
        engine.approve(&forged_digest),
        Err(PolicyError::ApprovalFingerprintMismatch)
    );

    let mut changed_request = approval.clone();
    changed_request.request.arguments_digest = "different-arguments".into();
    assert_eq!(
        engine.approve(&changed_request),
        Err(PolicyError::ApprovalFingerprintMismatch)
    );

    let mut changed_summary = approval.clone();
    changed_summary.summary = "harmless read-only operation".into();
    assert_eq!(
        engine.approve(&changed_summary),
        Err(PolicyError::ApprovalSummaryMismatch)
    );

    let grant = engine.approve(&approval).unwrap();
    let expected = approval_fingerprint(&original_request).unwrap();
    assert_eq!(grant.fingerprint, expected);
    assert_eq!(engine.consume(&grant, &expected), Ok(()));
}

#[test]
fn approve_rejects_fabricated_or_now_denied_requests() {
    let engine = engine();
    let read = request(
        PolicyMode::Ask,
        vec![ToolCapability::FileRead],
        SideEffect::ReadOnly,
        true,
    );
    let read_approval = lato_core::ApprovalRequest {
        fingerprint: approval_fingerprint(&read).unwrap(),
        request: read,
        summary: "forged".into(),
    };
    assert_eq!(
        engine.approve(&read_approval),
        Err(PolicyError::ApprovalNotRequired)
    );

    let denied = request(
        PolicyMode::Ask,
        vec![ToolCapability::ExtensionInvoke],
        SideEffect::ReadOnly,
        false,
    );
    let denied_approval = lato_core::ApprovalRequest {
        fingerprint: approval_fingerprint(&denied).unwrap(),
        request: denied,
        summary: "forged".into(),
    };
    assert_eq!(
        engine.approve(&denied_approval),
        Err(PolicyError::Denied("policy.untrusted_extension".into()))
    );
}

#[test]
fn policy_errors_have_stable_codes() {
    assert_eq!(
        PolicyError::ApprovalFingerprintMismatch.code(),
        "policy.grant_mismatch"
    );
    assert_eq!(
        PolicyError::ApprovalNotRequired.code(),
        "policy.approval_not_required"
    );
    assert_eq!(
        PolicyError::ApprovalSummaryMismatch.code(),
        "policy.approval_request_mismatch"
    );
    assert_eq!(
        PolicyError::Denied("policy.untrusted_extension".into()).code(),
        "policy.untrusted_extension"
    );
}
