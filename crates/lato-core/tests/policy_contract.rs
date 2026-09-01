use lato_core::{
    DescriptorError, SideEffect, ToolCancellation, ToolCapability, ToolConcurrency, ToolDescriptor,
    ToolIdempotency, ToolLayer, ToolName, ToolSource,
};
use semver::Version;

fn descriptor(capabilities: Vec<ToolCapability>, side_effect: SideEffect) -> ToolDescriptor {
    ToolDescriptor {
        name: ToolName::parse("test:tool").unwrap(),
        version: Version::new(1, 0, 0),
        description: "test tool".into(),
        input_schema: serde_json::json!({"type":"object"}),
        capabilities,
        side_effect,
        concurrency: ToolConcurrency::Serial,
        idempotency: ToolIdempotency::NonIdempotent,
        timeout_ms: 1_000,
        max_output_bytes: 1_024,
        cancellation: ToolCancellation::Cooperative,
        source: ToolSource {
            layer: ToolLayer::User,
            id: "test".into(),
            replacement: None,
        },
    }
}

#[test]
fn file_write_cannot_claim_read_only_side_effect() {
    let error = descriptor(vec![ToolCapability::FileWrite], SideEffect::ReadOnly)
        .validate()
        .unwrap_err();
    assert_eq!(error, DescriptorError::CapabilitySideEffectMismatch);
}

#[test]
fn network_write_requires_external_mutation() {
    assert!(
        descriptor(
            vec![ToolCapability::NetworkWrite],
            SideEffect::WorkspaceMutation,
        )
        .validate()
        .is_err()
    );
}

#[test]
fn process_spawn_cannot_claim_no_side_effect() {
    assert_eq!(
        descriptor(vec![ToolCapability::ProcessSpawn], SideEffect::None).validate_policy_metadata(),
        Err(DescriptorError::CapabilitySideEffectMismatch)
    );
}

#[test]
fn legacy_capability_and_side_effect_spellings_are_accepted() {
    assert_eq!(
        serde_json::from_str::<ToolCapability>("\"process\"").unwrap(),
        ToolCapability::ProcessSpawn
    );
    assert_eq!(
        serde_json::from_str::<SideEffect>("\"workspace_write\"").unwrap(),
        SideEffect::WorkspaceMutation
    );
}

#[test]
fn ambiguous_legacy_network_capability_is_rejected() {
    assert!(serde_json::from_str::<ToolCapability>("\"network\"").is_err());
    assert_eq!(
        serde_json::from_str::<ToolCapability>("\"network_read\"").unwrap(),
        ToolCapability::NetworkRead
    );
}

#[test]
fn policy_decision_has_stable_tagged_json() {
    let value = serde_json::to_value(lato_core::PolicyDecision::Deny(
        lato_core::PolicyDenial::new("policy.denied", "denied"),
    ))
    .unwrap();
    assert_eq!(value["kind"], "deny");
    assert_eq!(value["value"]["code"], "policy.denied");
}
