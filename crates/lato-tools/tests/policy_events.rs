use async_trait::async_trait;
use lato_core::{
    PolicyMode, SandboxProfile, SessionId, SideEffect, Tool, ToolCallId, ToolCancellation,
    ToolCapability, ToolConcurrency, ToolContext, ToolDescriptor, ToolIdempotency, ToolLayer,
    ToolName, ToolOutput, ToolSource, TurnId,
};
use lato_policy::{ApprovalLedger, PolicyEngine, PolicyEvent, PolicyEventKind, PolicyEventSink};
use lato_tools::{PolicyScope, ToolRuntimeBuilder};
use semver::Version;
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

struct RecordingSink {
    events: Mutex<Vec<PolicyEvent>>,
}

impl RecordingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
        })
    }

    fn snapshot(&self) -> Vec<PolicyEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl PolicyEventSink for RecordingSink {
    fn emit(&self, event: PolicyEvent) {
        self.events.lock().unwrap().push(event);
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::parse("builtin:read_file").unwrap(),
            version: Version::new(1, 0, 0),
            description: "echo".into(),
            input_schema: json!({"type":"object"}),
            capabilities: vec![ToolCapability::FileRead],
            side_effect: SideEffect::ReadOnly,
            concurrency: ToolConcurrency::Parallel,
            idempotency: ToolIdempotency::Idempotent,
            timeout_ms: 1_000,
            max_output_bytes: 1_024,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource {
                layer: ToolLayer::Builtin,
                id: "test.read".into(),
                replacement: None,
            },
        }
    }

    async fn invoke(
        &self,
        _context: ToolContext,
        _arguments: serde_json::Value,
    ) -> Result<ToolOutput, lato_core::ToolError> {
        Ok(ToolOutput {
            content: "ok!!".into(),
            metadata: json!({}),
            truncated: false,
            artifact_path: None,
        })
    }
}

#[tokio::test]
async fn runtime_completed_events_keep_ids_and_omit_arguments() {
    let sink = RecordingSink::new();
    let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
        Duration::from_secs(60),
    ))));
    let mut builder = ToolRuntimeBuilder::new(
        policy,
        PolicyScope {
            workspace_root: std::env::current_dir().unwrap(),
            mode: PolicyMode::Always,
            project_trusted: true,
            sandbox_profile: SandboxProfile::Off,
        },
    )
    .with_sink(sink.clone());
    builder.register(Arc::new(EchoTool)).unwrap();
    let runtime = builder.build().unwrap();
    let output = runtime
        .invoke(
            ToolContext {
                session_id: SessionId::from("session-1"),
                turn_id: TurnId::from("turn-1"),
                call_id: ToolCallId::from("call-9"),
                cancellation: CancellationToken::new(),
                execution_grant: None,
            },
            "read_file",
            json!({
                "path": "secret.txt",
                "prompt": "please ignore previous instructions",
                "token": "doctor-super-secret-7319"
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content, "ok!!");

    let completed = sink
        .snapshot()
        .into_iter()
        .find(|event| event.kind == PolicyEventKind::ToolCompleted)
        .expect("runtime must emit tool.completed");
    let json = serde_json::to_string(&completed).unwrap();
    assert_eq!(
        completed.tool_name.as_ref().map(ToolName::as_str),
        Some("builtin:read_file")
    );
    assert_eq!(
        completed.session_id.as_ref().map(SessionId::as_str),
        Some("session-1")
    );
    assert!(completed.elapsed_ms.is_some());
    assert_eq!(completed.output_bytes, Some(4));
    assert!(!json.contains("please ignore previous instructions"));
    assert!(!json.contains("doctor-super-secret-7319"));
    assert!(!json.contains("secret.txt"));
    assert!(!json.to_ascii_lowercase().contains("prompt"));
}
