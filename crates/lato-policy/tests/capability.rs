use lato_core::{DescriptorError, ToolCapability, ToolDescriptor};
use lato_policy::{CapabilityError, validate_capability_subset, validate_descriptor};
use serde_json::json;

#[test]
fn policy_cannot_add_network_write() {
    let declared = vec![ToolCapability::NetworkRead];
    let effective = vec![ToolCapability::NetworkRead, ToolCapability::NetworkWrite];

    assert_eq!(
        validate_capability_subset(&declared, &effective),
        Err(CapabilityError::Expansion(ToolCapability::NetworkWrite))
    );
}

#[test]
fn policy_can_narrow_and_deduplicate_declared_capabilities() {
    let declared = vec![ToolCapability::FileRead, ToolCapability::FileWrite];
    let effective = vec![ToolCapability::FileRead, ToolCapability::FileRead];

    assert_eq!(validate_capability_subset(&declared, &effective), Ok(()));
}

#[test]
fn descriptor_validation_delegates_to_the_core_contract() {
    let descriptor: ToolDescriptor = serde_json::from_value(json!({
        "name": "test:write",
        "version": "1.0.0",
        "description": "test tool",
        "input_schema": {"type": "object"},
        "capabilities": ["file_write"],
        "side_effect": "read_only",
        "concurrency": "serial",
        "idempotency": "non_idempotent",
        "timeout_ms": 1000,
        "max_output_bytes": 1024,
        "cancellation": "cooperative",
        "source": {"layer": "user", "id": "test", "replacement": null}
    }))
    .unwrap();

    assert_eq!(
        validate_descriptor(&descriptor),
        Err(DescriptorError::CapabilitySideEffectMismatch)
    );
}

#[test]
fn legacy_network_spelling_remains_rejected() {
    let error = serde_json::from_value::<ToolCapability>(json!("network")).unwrap_err();
    assert!(error.to_string().contains("unknown variant"));
}
