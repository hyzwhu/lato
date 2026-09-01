use async_trait::async_trait;
use lato_core::{
    SessionId, SideEffect, Tool, ToolCallId, ToolCancellation, ToolCapability, ToolConcurrency,
    ToolContext, ToolDescriptor, ToolError, ToolIdempotency, ToolLayer, ToolName, ToolOutput,
    ToolReplacement, ToolSource, TurnId,
};
use lato_tools::{CatalogError, RegistrationOutcome, ToolCatalog};
use semver::Version;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct FakeTool {
    descriptor: ToolDescriptor,
}

#[async_trait]
impl Tool for FakeTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn invoke(
        &self,
        _context: ToolContext,
        _arguments: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: self.descriptor.name.to_string(),
            metadata: serde_json::json!({}),
            truncated: false,
            artifact_path: None,
        })
    }
}

fn tool(
    name: &str,
    major: u64,
    layer: ToolLayer,
    replacement: Option<ToolReplacement>,
) -> Arc<dyn Tool> {
    Arc::new(FakeTool {
        descriptor: ToolDescriptor {
            name: ToolName::parse(name).unwrap(),
            version: Version::new(major, 0, 0),
            description: format!("test tool {name}"),
            input_schema: serde_json::json!({"type": "object"}),
            capabilities: vec![ToolCapability::ExtensionInvoke],
            side_effect: SideEffect::None,
            concurrency: ToolConcurrency::Parallel,
            idempotency: ToolIdempotency::Idempotent,
            timeout_ms: 1_000,
            max_output_bytes: 1_024,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource {
                layer,
                id: format!("source-{layer:?}"),
                replacement,
            },
        },
    })
}

fn context() -> ToolContext {
    ToolContext {
        session_id: SessionId::from("session-1"),
        turn_id: TurnId::from("turn-1"),
        call_id: ToolCallId::from("tool-call-1"),
        cancellation: CancellationToken::new(),
        execution_grant: None,
    }
}

#[test]
fn catalog_lists_descriptors_in_qualified_name_order() {
    let mut catalog = ToolCatalog::new();
    catalog
        .register(tool("zeta:run", 1, ToolLayer::Builtin, None))
        .unwrap();
    catalog
        .register(tool("alpha:read", 1, ToolLayer::Builtin, None))
        .unwrap();
    let names: Vec<_> = catalog
        .descriptors()
        .into_iter()
        .map(|value| value.name.to_string())
        .collect();
    assert_eq!(names, ["alpha:read", "zeta:run"]);
}

#[test]
fn duplicate_without_replacement_is_rejected_atomically() {
    let mut catalog = ToolCatalog::new();
    catalog
        .register(tool("lato:read", 1, ToolLayer::Builtin, None))
        .unwrap();
    let error = catalog
        .register(tool("lato:read", 1, ToolLayer::User, None))
        .unwrap_err();
    assert!(matches!(error, CatalogError::ReplacementRequired { .. }));
    assert_eq!(
        catalog
            .descriptor(&"lato:read".parse().unwrap())
            .unwrap()
            .source
            .layer,
        ToolLayer::Builtin
    );
}

#[test]
fn lower_or_equal_layer_cannot_replace() {
    let mut catalog = ToolCatalog::new();
    let target = ToolName::parse("lato:read").unwrap();
    catalog
        .register(tool(target.as_str(), 1, ToolLayer::User, None))
        .unwrap();
    let replacement = Some(ToolReplacement {
        target,
        compatible_major: 1,
    });
    let error = catalog
        .register(tool("lato:read", 1, ToolLayer::User, replacement))
        .unwrap_err();
    assert!(matches!(error, CatalogError::LowerOrEqualLayer { .. }));
}

#[test]
fn replacement_target_and_major_must_match() {
    let mut catalog = ToolCatalog::new();
    catalog
        .register(tool("lato:read", 1, ToolLayer::Builtin, None))
        .unwrap();
    let wrong_target = Some(ToolReplacement {
        target: ToolName::parse("lato:write").unwrap(),
        compatible_major: 1,
    });
    assert!(matches!(
        catalog
            .register(tool("lato:read", 1, ToolLayer::User, wrong_target))
            .unwrap_err(),
        CatalogError::ReplacementTargetMismatch { .. }
    ));
    let wrong_major = Some(ToolReplacement {
        target: ToolName::parse("lato:read").unwrap(),
        compatible_major: 2,
    });
    assert!(matches!(
        catalog
            .register(tool("lato:read", 2, ToolLayer::User, wrong_major))
            .unwrap_err(),
        CatalogError::IncompatibleMajorVersion { .. }
    ));
}

#[test]
fn explicit_compatible_higher_layer_replacement_succeeds() {
    let mut catalog = ToolCatalog::new();
    let name = ToolName::parse("lato:read").unwrap();
    catalog
        .register(tool(name.as_str(), 1, ToolLayer::Builtin, None))
        .unwrap();
    let replacement = Some(ToolReplacement {
        target: name.clone(),
        compatible_major: 1,
    });
    assert_eq!(
        catalog
            .register(tool(name.as_str(), 1, ToolLayer::User, replacement))
            .unwrap(),
        RegistrationOutcome::Replaced,
    );
    assert_eq!(
        catalog.descriptor(&name).unwrap().source.layer,
        ToolLayer::User
    );
}

#[test]
fn rejected_replacement_does_not_mutate_the_existing_descriptor() {
    let mut catalog = ToolCatalog::new();
    let name = ToolName::parse("lato:read").unwrap();
    catalog
        .register(tool(name.as_str(), 1, ToolLayer::Builtin, None))
        .unwrap();
    let before = catalog.descriptor(&name).cloned().unwrap();
    let incompatible = Some(ToolReplacement {
        target: name.clone(),
        compatible_major: 2,
    });
    assert!(
        catalog
            .register(tool(name.as_str(), 2, ToolLayer::User, incompatible))
            .is_err()
    );
    assert_eq!(catalog.descriptor(&name), Some(&before));
}

#[tokio::test]
async fn resolved_tool_invokes_through_the_object_safe_membrane() {
    let mut catalog = ToolCatalog::new();
    let name = ToolName::parse("lato:read").unwrap();
    catalog
        .register(tool(name.as_str(), 1, ToolLayer::Builtin, None))
        .unwrap();
    let resolved = catalog.resolve(&name).unwrap();
    let output = resolved
        .invoke(context(), serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(output.content, name.as_str());
}
