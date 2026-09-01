use lato_core::{SessionId, ToolCallId, ToolContext, TurnId};
use lato_tools::{BuiltinToolEnvironment, builtin_tools};
use lato_workspace::{FileLocks, SessionTrust};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn context(cancellation: CancellationToken) -> ToolContext {
    ToolContext {
        session_id: SessionId::from("session-1"),
        turn_id: TurnId::from("turn-1"),
        call_id: ToolCallId::from("call-1"),
        cancellation,
        execution_grant: None,
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
    let mut expected = [
        "grep",
        "list_dir",
        "read_file",
        "run_terminal_command",
        "search_replace",
        "spawn_subagent",
        "todo_write",
        "web_fetch",
        "write_file",
    ]
    .map(str::to_owned)
    .to_vec();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 9);
    assert_eq!(
        actual
            .iter()
            .filter(|name| name.as_str() == "write_file")
            .count(),
        1,
        "the compatibility definition must not duplicate a registry definition"
    );
    let write_file = tools
        .iter()
        .map(|tool| tool.descriptor())
        .find(|descriptor| descriptor.name.as_str() == "builtin:write_file")
        .unwrap();
    assert_eq!(
        write_file.description,
        "Create or overwrite a UTF-8 file in the workspace. Use this to write new files such as hello.go. Prefer this over printing file contents in chat."
    );
    assert_eq!(
        write_file.input_schema,
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path relative to the workspace or absolute"
                },
                "contents": {
                    "type": "string",
                    "description": "Full file contents"
                }
            },
            "required": ["path", "contents"]
        })
    );
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
        capabilities: vec![lato_core::ToolCapability::ExtensionInvoke],
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

#[tokio::test]
async fn write_alias_consumes_allow_once_exactly_once() {
    let root = tempfile::tempdir().unwrap();
    let trust = SessionTrust::for_interactive(root.path(), true);
    trust.allow_once();
    let runtime = lato_tools::builtin_tool_runtime(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: trust.clone(),
    })
    .unwrap();

    runtime
        .invoke(
            context(CancellationToken::new()),
            "write",
            serde_json::json!({"path": "a.txt", "contents": "one"}),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "one"
    );
    assert!(!trust.has_allow_once());

    let error = runtime
        .invoke(
            context(CancellationToken::new()),
            "write_file",
            serde_json::json!({"path": "b.txt", "contents": "two"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "tool.policy_denied");
    assert!(!root.path().join("b.txt").exists());
}

#[tokio::test]
async fn malformed_and_denied_calls_are_classified() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "SECRET=1").unwrap();
    let runtime = lato_tools::builtin_tool_runtime(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(root.path()),
    })
    .unwrap();

    let malformed = runtime
        .invoke(
            context(CancellationToken::new()),
            "read_file",
            serde_json::json!({}),
        )
        .await
        .unwrap_err();
    assert_eq!(malformed.code, "tool.invalid_arguments");

    let denied = runtime
        .invoke(
            context(CancellationToken::new()),
            "write_file",
            serde_json::json!({"path": ".env", "contents": "SECRET=2"}),
        )
        .await
        .unwrap_err();
    assert_eq!(denied.code, "tool.policy_denied");
    assert_eq!(
        std::fs::read_to_string(root.path().join(".env")).unwrap(),
        "SECRET=1"
    );
}

#[test]
fn every_advertised_tool_has_one_executable_descriptor() {
    let root = tempfile::tempdir().unwrap();
    let runtime = lato_tools::builtin_tool_runtime(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(root.path()),
    })
    .unwrap();

    for definition in runtime.model_definitions() {
        let name = definition
            .pointer("/function/name")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert_eq!(
            runtime
                .descriptor_for_wire_name(name)
                .unwrap()
                .name
                .local_name(),
            name
        );
    }
}
