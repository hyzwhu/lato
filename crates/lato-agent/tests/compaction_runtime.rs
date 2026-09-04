use async_trait::async_trait;
use lato_agent::{AcpHost, REQUIRED_SECTIONS};
use lato_ai::{ModelStream, StreamPiece};
use lato_protocol::JsonRpcReq;
use lato_workspace::SessionTrust;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

struct RecordingStream {
    scripts: tokio::sync::Mutex<Vec<Vec<StreamPiece>>>,
    contexts: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait]
impl ModelStream for RecordingStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), String> {
        self.contexts.lock().unwrap().push(context);
        let script = self.scripts.lock().await.remove(0);
        for piece in script {
            tx.send(piece).await.map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

fn req(id: i32, method: &str, params: serde_json::Value) -> JsonRpcReq {
    JsonRpcReq {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::json!(id)),
        method: method.into(),
        params: Some(params),
    }
}

fn summary() -> String {
    let detail = "preserve verified implementation state, decisions, and pending work ".repeat(2);
    REQUIRED_SECTIONS
        .iter()
        .enumerate()
        .map(|(index, heading)| format!("{}. {}: {detail}", index + 1, heading))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[tokio::test]
async fn persisted_compaction_rebuilds_after_cache_corruption_and_continues() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let trust = SessionTrust::for_headless_prompt(workspace.path());
    let first_contexts = Arc::new(Mutex::new(Vec::new()));
    let first_stream: Arc<dyn ModelStream> = Arc::new(RecordingStream {
        scripts: tokio::sync::Mutex::new(vec![
            vec![StreamPiece::Text(
                "SECRET_RAW_PRECOMPACTION_PAYLOAD ".repeat(1_000),
            )],
            vec![StreamPiece::Text(summary())],
        ]),
        contexts: first_contexts,
    });
    let (updates, _updates_rx) = mpsc::unbounded_channel();
    let mut first = AcpHost::new_with_home(
        workspace.path().to_path_buf(),
        trust.clone(),
        updates,
        first_stream,
        home.path().to_path_buf(),
    );
    let created = first
        .handle(req(1, "session/new", serde_json::json!({})))
        .await
        .unwrap();
    let sid = created["result"]["sessionId"].as_str().unwrap().to_owned();
    let prompted = first
        .handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "finish the parser"}),
        ))
        .await
        .unwrap();
    assert_eq!(prompted["result"]["status"], "complete");
    let compacted = first
        .handle(req(
            3,
            "lato/session/compact",
            serde_json::json!({"sessionId": sid}),
        ))
        .await
        .unwrap();
    assert_eq!(compacted["result"]["status"], "complete");
    first
        .handle(req(
            4,
            "session/close",
            serde_json::json!({"sessionId": sid}),
        ))
        .await
        .unwrap();
    drop(first);

    std::fs::write(
        home.path()
            .join("sessions")
            .join(&sid)
            .join("history.jsonl"),
        b"damaged cache\n",
    )
    .unwrap();

    let resumed_contexts = Arc::new(Mutex::new(Vec::new()));
    let resumed_stream: Arc<dyn ModelStream> = Arc::new(RecordingStream {
        scripts: tokio::sync::Mutex::new(vec![vec![StreamPiece::Text("continued".into())]]),
        contexts: resumed_contexts.clone(),
    });
    let (updates, _updates_rx) = mpsc::unbounded_channel();
    let mut resumed = AcpHost::new_with_home(
        workspace.path().to_path_buf(),
        trust,
        updates,
        resumed_stream,
        home.path().to_path_buf(),
    );
    let response = resumed
        .handle(req(
            5,
            "session/resume",
            serde_json::json!({"sessionId": sid}),
        ))
        .await
        .unwrap();
    assert!(response.get("error").is_none(), "{response}");
    let response = resumed
        .handle(req(
            6,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "continue now"}),
        ))
        .await
        .unwrap();
    assert_eq!(response["result"]["status"], "complete");

    let contexts = resumed_contexts.lock().unwrap();
    let messages = contexts.last().unwrap()["messages"].as_array().unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "system")
            .count(),
        1
    );
    let encoded = serde_json::to_string(messages).unwrap();
    assert!(encoded.contains("<user_query>"));
    assert!(encoded.contains("<conversation_summary version=\\\"1\\\">"));
    assert!(encoded.contains("continue now"));
    assert!(!encoded.contains("SECRET_RAW_PRECOMPACTION_PAYLOAD"));
}
