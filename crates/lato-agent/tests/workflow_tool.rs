// Phase 7B7: main-session registration of the model-visible `workflow` tool.

use std::sync::Arc;

use lato_agent::AcpHost;
use lato_ai::{ModelStream, StreamPiece};
use lato_core::{ModelError, ToolCapability};
use lato_protocol::JsonRpcReq;
use lato_workspace::{FileLocks, SessionTrust as Trust};
use tokio::sync::mpsc;

struct ScriptedStream {
    rounds: tokio::sync::Mutex<std::collections::VecDeque<Vec<StreamPiece>>>,
    contexts: tokio::sync::Mutex<Vec<serde_json::Value>>,
}

impl ScriptedStream {
    fn scripted(rounds: Vec<Vec<StreamPiece>>) -> Self {
        Self {
            rounds: tokio::sync::Mutex::new(rounds.into()),
            contexts: tokio::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl ModelStream for ScriptedStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError> {
        self.contexts.lock().await.push(context);
        let pieces = self
            .rounds
            .lock()
            .await
            .pop_front()
            .unwrap_or_else(|| vec![StreamPiece::Text("done".into())]);
        for piece in pieces {
            tx.send(piece).await.map_err(|_| ModelError::cancelled())?;
        }
        Ok(())
    }
}

fn host(
    stream: Arc<dyn ModelStream>,
) -> (
    AcpHost,
    tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
) {
    let cwd = std::env::current_dir().unwrap();
    let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
    (
        AcpHost::new(
            cwd.clone(),
            Trust::for_headless_prompt(&cwd),
            updates_tx,
            stream,
        ),
        updates_rx,
    )
}

fn req(id: i32, method: &str, params: serde_json::Value) -> JsonRpcReq {
    JsonRpcReq {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::json!(id)),
        method: method.into(),
        params: Some(params),
    }
}

/// AC-01: the main-session model catalog carries exactly one `workflow` tool
/// with the §4 schema, and a scripted model turn can drive it end-to-end
/// through the ordinary ToolRuntime membrane.
#[tokio::test]
async fn main_session_turn_can_drive_the_workflow_tool() {
    let stream = Arc::new(ScriptedStream::scripted(vec![
        vec![StreamPiece::ToolCall {
            id: "call-1".into(),
            name: "workflow".into(),
            arguments: serde_json::json!({"action":"list"}),
        }],
        vec![StreamPiece::Text("done".into())],
    ]));
    let (mut acp, _updates) = host(stream.clone());
    let sid = acp
        .handle(req(1, "session/new", serde_json::json!({})))
        .await
        .unwrap()["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let response = acp
        .handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "list workflows"}),
        ))
        .await
        .unwrap();
    assert_eq!(response["result"]["status"], "complete");

    // Round 2's model context must contain the tool result: a bounded,
    // structured `workflow` list payload produced by the session-bound tool.
    let contexts = stream.contexts.lock().await.clone();
    assert!(contexts.len() >= 2, "expected a second model round");
    let messages = contexts[1]["messages"].as_array().unwrap().clone();
    let tool_message = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("workflow tool result reached the model");
    let result: serde_json::Value = serde_json::from_str(tool_message["content"].as_str().unwrap())
        .expect("tool result is structured JSON");
    assert_eq!(result["action"], "list");
    assert!(result["workflows"].is_array());
    assert_eq!(result["truncated"], false);
    // The catalog itself carries exactly one `workflow` wire name.
    let catalog = serde_json::to_string(&contexts[0]).unwrap();
    assert_eq!(catalog.matches("\"name\":\"workflow\"").count(), 1);
    assert!(!catalog.contains("script.rhai"));
}

/// AC-09: subagent/headless capability ceilings never see the `workflow` tool
/// — the extra-tool slot is only used by the main-session host wiring.
#[test]
fn capability_filtered_catalogs_exclude_the_workflow_tool() {
    let cwd = std::env::current_dir().unwrap();
    let runtime = lato_tools::builtin_tool_runtime_for_capabilities(
        lato_tools::BuiltinToolEnvironment {
            cwd: cwd.clone(),
            locks: Arc::new(FileLocks::new()),
            trust: Trust::for_headless_prompt(&cwd),
            skill_resolver: None,
        },
        Some(&[ToolCapability::TaskControl, ToolCapability::FileRead]),
    )
    .unwrap();
    assert!(
        !runtime
            .model_definitions()
            .iter()
            .any(|definition| definition["function"]["name"] == "workflow")
    );
}

/// AC-09 (wiring guard): only the main-session host path consumes the
/// extra-tools slot; subagent and headless builders must stay plain.
#[test]
fn only_the_main_session_host_registers_the_workflow_tool() {
    let host_source = include_str!("../src/host.rs");
    assert!(host_source.contains("builtin_tool_runtime_with_subagents_and_mcp_extra"));
    assert!(host_source.contains("workflow_handle.install(manager)"));

    let runner_source = include_str!("../src/subagent/runner.rs");
    assert!(!runner_source.contains("subagents_and_mcp_extra"));
    let skills_source = include_str!("../src/skills.rs");
    assert!(!skills_source.contains("subagents_and_mcp_extra"));
}
