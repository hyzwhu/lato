use lato_core::{SessionId, ToolCallId, ToolContext, TurnId};
use lato_tools::{BuiltinToolEnvironment, builtin_tools, v1_tool_definitions};
use lato_workspace::{FileLocks, SessionTrust};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn context(cancellation: CancellationToken) -> ToolContext {
    ToolContext {
        session_id: SessionId::from("session-1"),
        turn_id: TurnId::from("turn-1"),
        call_id: ToolCallId::from("call-1"),
        cancellation,
    }
}

#[test]
fn builtin_adapters_match_the_v1_model_definition_set() {
    let root = tempfile::tempdir().unwrap();
    let tools = builtin_tools(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(root.path()),
    })
    .unwrap();
    let mut actual = tools
        .iter()
        .map(|tool| tool.descriptor().name.local_name().to_owned())
        .collect::<Vec<_>>();
    let mut expected = v1_tool_definitions()
        .as_array()
        .unwrap()
        .iter()
        .map(|definition| {
            definition
                .pointer("/function/name")
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 9);
}

#[tokio::test]
async fn cancelled_adapter_does_not_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let tools = builtin_tools(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(root.path()),
    })
    .unwrap();
    let read = tools
        .into_iter()
        .find(|tool| tool.descriptor().name.as_str() == "builtin:read_file")
        .unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = read
        .invoke(
            context(cancellation),
            serde_json::json!({"path": "missing"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "tool.cancelled");
}

#[tokio::test]
async fn runtime_advertises_and_executes_the_same_tools() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "hello").unwrap();
    let runtime = lato_tools::builtin_tool_runtime(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(root.path()),
    })
    .unwrap();
    let definitions = runtime.model_definitions();
    assert_eq!(definitions.len(), 9);
    for name in ["read_file", "Lato:read_file", "builtin:read_file"] {
        let output = runtime
            .invoke(
                context(CancellationToken::new()),
                name,
                serde_json::json!({"path": "a.txt"}),
            )
            .await
            .unwrap();
        assert_eq!(output.content, "hello");
    }
}

#[tokio::test]
async fn unknown_wire_name_is_typed() {
    let root = tempfile::tempdir().unwrap();
    let runtime = lato_tools::builtin_tool_runtime(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(root.path()),
    })
    .unwrap();
    let error = runtime
        .invoke(
            context(CancellationToken::new()),
            "missing",
            serde_json::json!({}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "tool.not_found");
}

struct FakeTool {
    descriptor: lato_core::ToolDescriptor,
    output: String,
}

#[async_trait::async_trait]
impl lato_core::Tool for FakeTool {
    fn descriptor(&self) -> lato_core::ToolDescriptor {
        self.descriptor.clone()
    }

    async fn invoke(
        &self,
        _context: ToolContext,
        _arguments: serde_json::Value,
    ) -> Result<lato_core::ToolOutput, lato_core::ToolError> {
        Ok(lato_core::ToolOutput {
            content: self.output.clone(),
            metadata: serde_json::json!({}),
            truncated: false,
            artifact_path: None,
        })
    }
}

fn fake_descriptor(
    name: &str,
    layer: lato_core::ToolLayer,
    replacement: Option<lato_core::ToolReplacement>,
) -> lato_core::ToolDescriptor {
    lato_core::ToolDescriptor {
        name: lato_core::ToolName::parse(name).unwrap(),
        version: semver::Version::new(1, 0, 0),
        description: format!("fake {name}"),
        input_schema: serde_json::json!({"type": "object"}),
        capabilities: vec![lato_core::ToolCapability::Other("test".into())],
        side_effect: lato_core::SideEffect::None,
        concurrency: lato_core::ToolConcurrency::Parallel,
        idempotency: lato_core::ToolIdempotency::Idempotent,
        timeout_ms: 1_000,
        max_output_bytes: 1_024,
        cancellation: lato_core::ToolCancellation::Cooperative,
        source: lato_core::ToolSource {
            layer,
            id: format!("test.{name}"),
            replacement,
        },
    }
}

#[tokio::test]
async fn legacy_write_aliases_resolve_to_write_file() {
    let mut builder = lato_tools::ToolRuntimeBuilder::new();
    builder
        .register(Arc::new(FakeTool {
            descriptor: fake_descriptor("builtin:write_file", lato_core::ToolLayer::Builtin, None),
            output: "write-file".into(),
        }))
        .unwrap();
    let runtime = builder.build().unwrap();

    for name in ["write", "Lato:write"] {
        let output = runtime
            .invoke(
                context(CancellationToken::new()),
                name,
                serde_json::json!({}),
            )
            .await
            .unwrap();
        assert_eq!(output.content, "write-file");
    }
}

#[tokio::test]
async fn higher_layer_replaces_a_builtin_without_duplicate_advertisement() {
    let root = tempfile::tempdir().unwrap();
    let mut builder = lato_tools::ToolRuntimeBuilder::new();
    builder
        .register_builtin_tools(BuiltinToolEnvironment {
            cwd: root.path().to_path_buf(),
            locks: Arc::new(FileLocks::new()),
            trust: SessionTrust::for_headless_prompt(root.path()),
        })
        .unwrap();
    let target = lato_core::ToolName::parse("builtin:read_file").unwrap();
    builder
        .register(Arc::new(FakeTool {
            descriptor: fake_descriptor(
                target.as_str(),
                lato_core::ToolLayer::SessionOverride,
                Some(lato_core::ToolReplacement {
                    target: target.clone(),
                    compatible_major: 1,
                }),
            ),
            output: "replacement".into(),
        }))
        .unwrap();
    let runtime = builder.build().unwrap();
    assert_eq!(
        runtime
            .model_definitions()
            .iter()
            .filter(|value| {
                value
                    .pointer("/function/name")
                    .and_then(|name| name.as_str())
                    == Some("read_file")
            })
            .count(),
        1
    );
    let output = runtime
        .invoke(
            context(CancellationToken::new()),
            "read_file",
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert_eq!(output.content, "replacement");
}

#[test]
fn different_namespaces_cannot_advertise_the_same_local_name() {
    let mut builder = lato_tools::ToolRuntimeBuilder::new();
    for name in ["project:read", "session:read"] {
        builder
            .register(Arc::new(FakeTool {
                descriptor: fake_descriptor(name, lato_core::ToolLayer::Builtin, None),
                output: name.into(),
            }))
            .unwrap();
    }
    let error = match builder.build() {
        Ok(_) => panic!("ambiguous wire names must fail the build"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        lato_tools::RuntimeBuildError::AmbiguousWireName { wire_name, .. }
            if wire_name == "read"
    ));
}
