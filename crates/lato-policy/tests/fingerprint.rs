use lato_core::{
    EnvironmentPolicy, NetworkPolicy, PolicyMode, PolicyRequest, SandboxObligation, SandboxProfile,
    SessionId, SideEffect, ToolCallId, ToolCapability, ToolLayer, ToolName, TurnId,
};
use lato_policy::{approval_fingerprint, canonical_arguments};
use serde_json::json;
use std::path::PathBuf;

fn request() -> PolicyRequest {
    PolicyRequest {
        session_id: SessionId::from("session-1"),
        turn_id: TurnId::from("turn-1"),
        call_id: ToolCallId::from("call-1"),
        tool_name: ToolName::parse("builtin:write_file").unwrap(),
        arguments_digest: "arguments-digest-1".into(),
        capabilities: vec![ToolCapability::FileWrite],
        side_effect: SideEffect::WorkspaceMutation,
        mode: PolicyMode::Ask,
        project_trusted: true,
        sandbox: SandboxObligation {
            profile: SandboxProfile::Workspace,
            workspace_root: PathBuf::from("/workspace"),
            writable_roots: vec![PathBuf::from("/workspace")],
            network: NetworkPolicy::Deny,
            environment: EnvironmentPolicy {
                allowed_keys: vec!["PATH".into()],
            },
        },
        detail: None,
        plan_mode: false,
        tool_layer: ToolLayer::Builtin,
    }
}

#[test]
fn object_key_order_does_not_change_canonical_arguments() {
    let a = json!({"path":"a", "options":{"b":2,"a":1}});
    let b = json!({"options":{"a":1,"b":2}, "path":"a"});

    assert_eq!(
        canonical_arguments(&a).unwrap(),
        canonical_arguments(&b).unwrap()
    );
}

#[test]
fn canonical_arguments_preserve_array_order_and_scalar_values() {
    let value = json!({"z":[2, 1], "number":1.5, "string":"1.5"});

    assert_eq!(
        canonical_arguments(&value).unwrap(),
        br#"{"number":1.5,"string":"1.5","z":[2,1]}"#
    );
    assert_ne!(
        canonical_arguments(&json!({"z":[2, 1]})).unwrap(),
        canonical_arguments(&json!({"z":[1, 2]})).unwrap()
    );
}

#[test]
fn fingerprint_commits_to_every_exact_call_field() {
    let baseline = request();
    let original = approval_fingerprint(&baseline).unwrap();

    let mut variants = Vec::new();

    let mut changed = baseline.clone();
    changed.session_id = SessionId::from("session-2");
    variants.push(changed);

    let mut changed = baseline.clone();
    changed.turn_id = TurnId::from("turn-2");
    variants.push(changed);

    let mut changed = baseline.clone();
    changed.call_id = ToolCallId::from("call-2");
    variants.push(changed);

    let mut changed = baseline.clone();
    changed.tool_name = ToolName::parse("builtin:edit_file").unwrap();
    variants.push(changed);

    let mut changed = baseline.clone();
    changed.arguments_digest = "arguments-digest-2".into();
    variants.push(changed);

    let mut changed = baseline.clone();
    changed.capabilities.push(ToolCapability::FileRead);
    variants.push(changed);

    let mut changed = baseline.clone();
    changed.side_effect = SideEffect::ExternalMutation;
    variants.push(changed);

    let mut changed = baseline.clone();
    changed.mode = PolicyMode::Always;
    variants.push(changed);

    let mut changed = baseline.clone();
    changed.project_trusted = false;
    variants.push(changed);

    let mut changed = baseline;
    changed.sandbox.profile = SandboxProfile::ReadOnly;
    variants.push(changed);

    for changed in variants {
        assert_ne!(original, approval_fingerprint(&changed).unwrap());
    }
}

#[test]
fn fingerprint_sorts_and_deduplicates_capabilities() {
    let mut a = request();
    a.capabilities = vec![ToolCapability::FileWrite, ToolCapability::FileRead];
    let mut b = a.clone();
    b.capabilities = vec![
        ToolCapability::FileRead,
        ToolCapability::FileWrite,
        ToolCapability::FileRead,
    ];

    assert_eq!(
        approval_fingerprint(&a).unwrap(),
        approval_fingerprint(&b).unwrap()
    );
}

#[test]
fn fingerprint_is_lowercase_sha256() {
    let fingerprint = approval_fingerprint(&request()).unwrap();

    assert_eq!(
        fingerprint.0,
        // Golden for fingerprint domain v2 (PolicyRequest gained detail,
        // plan_mode and tool_layer bindings); recomputed from the same
        // canonical request.
        "87c03d042d274167cbb2168444457c3495e40c8945b1d3bd5ad99bdb37c3645c"
    );
    assert_eq!(fingerprint.0.len(), 64);
    assert!(
        fingerprint
            .0
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
}

#[test]
fn fingerprint_commits_to_the_full_sandbox_obligation() {
    let baseline = request();
    let original = approval_fingerprint(&baseline).unwrap();

    let mut changed = baseline.clone();
    changed.sandbox.workspace_root = PathBuf::from("/other-workspace");
    assert_ne!(original, approval_fingerprint(&changed).unwrap());

    let mut changed = baseline.clone();
    changed.sandbox.writable_roots.push(PathBuf::from("/tmp"));
    assert_ne!(original, approval_fingerprint(&changed).unwrap());

    let mut changed = baseline.clone();
    changed.sandbox.network = NetworkPolicy::PublicHttpsRead;
    assert_ne!(original, approval_fingerprint(&changed).unwrap());

    let mut changed = baseline;
    changed.sandbox.environment.allowed_keys.push("LANG".into());
    assert_ne!(original, approval_fingerprint(&changed).unwrap());
}
