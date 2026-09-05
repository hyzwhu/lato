use async_trait::async_trait;
use lato_agent::{AcpHost, REQUIRED_SECTIONS};
use lato_ai::{ModelMetadata, ModelStream, StreamPiece, adapt_model_endpoint};
use lato_core::{ModelError, Retryability};
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
    ) -> Result<(), ModelError> {
        self.contexts.lock().unwrap().push(context);
        let script = self.scripts.lock().await.remove(0);
        for piece in script {
            tx.send(piece).await.map_err(|_| {
                ModelError::new(
                    "model.receiver_closed",
                    "model stream receiver closed",
                    Retryability::Never,
                )
            })?;
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

fn endpoint(stream: Arc<dyn ModelStream>) -> Arc<dyn ModelStream> {
    adapt_model_endpoint(
        "fixture",
        "automatic",
        ModelMetadata {
            context_window: Some(120_000),
            model_family: Some("fixture".into()),
        },
        stream,
    )
    .unwrap()
    .stream
}

#[tokio::test]
async fn automatic_compaction_is_checkpointed_and_survives_restart() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let trust = SessionTrust::for_headless_prompt(workspace.path());
    let first_contexts = Arc::new(Mutex::new(Vec::new()));
    let first_stream: Arc<dyn ModelStream> = Arc::new(RecordingStream {
        scripts: tokio::sync::Mutex::new(vec![
            vec![StreamPiece::Text(
                "SECRET_AUTOMATIC_RECOVERY_PAYLOAD ".repeat(12_000),
            )],
            vec![StreamPiece::Text(summary())],
            vec![StreamPiece::Text("continued before restart".into())],
        ]),
        contexts: first_contexts,
    });
    let (updates, mut updates_rx) = mpsc::unbounded_channel();
    let mut first = AcpHost::new_with_home(
        workspace.path().to_path_buf(),
        trust.clone(),
        updates,
        endpoint(first_stream),
        home.path().to_path_buf(),
    );
    let created = first
        .handle(req(1, "session/new", serde_json::json!({})))
        .await
        .unwrap();
    let sid = created["result"]["sessionId"].as_str().unwrap().to_owned();
    for (id, text) in [(2, "retain this work"), (3, "continue now")] {
        let response = first
            .handle(req(
                id,
                "session/prompt",
                serde_json::json!({"sessionId":sid,"text":text}),
            ))
            .await
            .unwrap();
        assert_eq!(response["result"]["status"], "complete");
    }
    let updates = std::iter::from_fn(|| updates_rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(updates.iter().any(|update| {
        update["method"] == "lato/session/compaction"
            && update["params"]["event"] == "started"
            && update["params"]["trigger"] == "threshold"
    }));
    first
        .handle(req(
            4,
            "session/close",
            serde_json::json!({"sessionId":sid}),
        ))
        .await
        .unwrap();
    drop(first);

    let journal_path = home.path().join("sessions").join(&sid).join("events.jsonl");
    let records = std::fs::read_to_string(&journal_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let requested = records
        .iter()
        .position(|record| record["record"]["type"] == "compaction_requested")
        .unwrap();
    let checkpoint = records
        .iter()
        .position(|record| record["record"]["type"] == "history_projection_replaced")
        .unwrap();
    assert!(requested < checkpoint);

    let resumed_contexts = Arc::new(Mutex::new(Vec::new()));
    let resumed_stream: Arc<dyn ModelStream> = Arc::new(RecordingStream {
        scripts: tokio::sync::Mutex::new(vec![vec![StreamPiece::Text(
            "continued after restart".into(),
        )]]),
        contexts: resumed_contexts.clone(),
    });
    let (updates, _) = mpsc::unbounded_channel();
    let mut resumed = AcpHost::new_with_home(
        workspace.path().to_path_buf(),
        trust,
        updates,
        endpoint(resumed_stream),
        home.path().to_path_buf(),
    );
    let response = resumed
        .handle(req(
            5,
            "session/resume",
            serde_json::json!({"sessionId":sid}),
        ))
        .await
        .unwrap();
    assert!(response.get("error").is_none(), "{response}");
    let response = resumed
        .handle(req(
            6,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"resume the task"}),
        ))
        .await
        .unwrap();
    assert_eq!(response["result"]["text"], "continued after restart");
    let encoded = resumed_contexts.lock().unwrap()[0].to_string();
    assert!(encoded.contains("conversation_summary"));
    assert!(!encoded.contains("SECRET_AUTOMATIC_RECOVERY_PAYLOAD"));
    let records = std::fs::read_to_string(&journal_path).unwrap();
    let resumed_completion = records
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let record = serde_json::from_str::<serde_json::Value>(line).ok()?;
            (record["record"]["type"] == "turn_completed").then_some(index)
        })
        .last()
        .unwrap();
    assert!(checkpoint < resumed_completion);
}
