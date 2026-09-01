use async_trait::async_trait;
use lato_core::{
    DescriptorError, ErrorCategory, Retryability, SideEffect, Tool, ToolCancellation,
    ToolCapability, ToolConcurrency, ToolContext, ToolDescriptor, ToolError, ToolIdempotency,
    ToolLayer, ToolName, ToolOutput, ToolReplacement, ToolSource,
};
use semver::Version;
use serde_json::Value;
use std::sync::Arc;

fn descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: ToolName::parse("lato:read_file").unwrap(),
        version: Version::new(1, 2, 0),
        description: "Read a UTF-8 file".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
        capabilities: vec![ToolCapability::FileRead],
        side_effect: SideEffect::WorkspaceRead,
        concurrency: ToolConcurrency::Parallel,
        idempotency: ToolIdempotency::Idempotent,
        timeout_ms: 30_000,
        max_output_bytes: 64 * 1024,
        cancellation: ToolCancellation::Cooperative,
        source: ToolSource {
            layer: ToolLayer::Builtin,
            id: "lato".into(),
            replacement: None,
        },
    }
}

#[test]
fn qualified_names_and_descriptors_have_stable_json() {
    let descriptor = descriptor();
    descriptor.validate().unwrap();
    let value = serde_json::to_value(descriptor).unwrap();
    assert_eq!(value["name"], "lato:read_file");
    assert_eq!(value["version"], "1.2.0");
    assert_eq!(value["side_effect"], "workspace_read");
    assert_eq!(value["source"]["layer"], "builtin");
}

#[test]
fn tool_names_are_strictly_qualified() {
    assert!(ToolName::parse("read_file").is_err());
    assert!(ToolName::parse(":read_file").is_err());
    assert!(ToolName::parse("lato:read file").is_err());
    assert_eq!(
        ToolName::parse("lato:read_file").unwrap().namespace(),
        "lato"
    );
}

#[test]
fn descriptor_validation_rejects_unsafe_empty_limits() {
    let mut value = descriptor();
    value.timeout_ms = 0;
    assert_eq!(value.validate(), Err(DescriptorError::ZeroTimeout));
    value.timeout_ms = 1;
    value.max_output_bytes = 0;
    assert_eq!(value.validate(), Err(DescriptorError::ZeroOutputLimit));
}

#[test]
fn descriptor_rejects_duplicate_capabilities() {
    let mut value = descriptor();
    value.capabilities.push(ToolCapability::FileRead);
    assert_eq!(value.validate(), Err(DescriptorError::DuplicateCapability));
}

#[test]
fn custom_capabilities_round_trip_without_erasing_their_name() {
    let capability = ToolCapability::Other("database_read".into());
    let encoded = serde_json::to_string(&capability).unwrap();
    let decoded: ToolCapability = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, capability);
}

#[test]
fn tool_errors_convert_without_string_classification() {
    let error = ToolError::new(
        "tool.invalid_arguments",
        "path is required",
        Retryability::Never,
    );
    let agent: lato_core::AgentError = error.into();
    assert_eq!(agent.category, ErrorCategory::Tool);
    assert_eq!(agent.code, "tool.invalid_arguments");
}

#[test]
fn replacement_shape_is_explicit() {
    let replacement = ToolReplacement {
        target: ToolName::parse("lato:read_file").unwrap(),
        compatible_major: 1,
    };
    assert_eq!(
        serde_json::to_value(replacement).unwrap()["compatible_major"],
        1
    );
}

struct ExampleTool;

#[async_trait]
impl Tool for ExampleTool {
    fn descriptor(&self) -> ToolDescriptor {
        descriptor()
    }

    async fn invoke(
        &self,
        _context: ToolContext,
        _arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        unreachable!("compile-only object-safety test")
    }
}

#[test]
fn tool_contract_is_object_safe() {
    let tool: Arc<dyn Tool> = Arc::new(ExampleTool);
    assert_eq!(tool.descriptor().name.as_str(), "lato:read_file");
}
