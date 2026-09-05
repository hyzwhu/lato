use async_trait::async_trait;
use lato_agent::{AcpHost, REQUIRED_SECTIONS};
use lato_ai::{ModelMetadata, ModelStream, StreamPiece, adapt_model_endpoint};
use lato_core::{ModelError, ModelErrorKind, Retryability};
use lato_protocol::JsonRpcReq;
use lato_workspace::SessionTrust;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

struct RecordingStream {
    scripts: tokio::sync::Mutex<Vec<Vec<StreamPiece>>>,
    contexts: Arc<Mutex<Vec<serde_json::Value>>>,
}

struct RecoveryStream {
    scripts: tokio::sync::Mutex<Vec<Result<Vec<StreamPiece>, ModelError>>>,
    contexts: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait]
impl ModelStream for RecoveryStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError> {
        self.contexts.lock().unwrap().push(context);
        let script = self.scripts.lock().await.remove(0)?;
        for piece in script {
            tx.send(piece).await.map_err(|_| ModelError::cancelled())?;
        }
        Ok(())
    }
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

#[tokio::test]
async fn automatic_compaction_runs_before_the_next_sample_and_rebuilds_its_request() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let trust = SessionTrust::for_headless_prompt(workspace.path());
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let raw_payload = "SECRET_AUTOMATIC_PRECOMPACTION_PAYLOAD ".repeat(11_000);
    let raw: Arc<dyn ModelStream> = Arc::new(RecordingStream {
        scripts: tokio::sync::Mutex::new(vec![
            vec![StreamPiece::Text(raw_payload.clone())],
            vec![StreamPiece::Text(summary())],
            vec![StreamPiece::Text("continued".into())],
        ]),
        contexts: contexts.clone(),
    });
    let endpoint = adapt_model_endpoint(
        "fixture",
        "automatic",
        ModelMetadata {
            context_window: Some(120_000),
            model_family: Some("fixture".into()),
        },
        raw,
    )
    .unwrap();
    let (updates, _updates_rx) = mpsc::unbounded_channel();
    let mut host = AcpHost::new_with_home(
        workspace.path().to_path_buf(),
        trust,
        updates,
        endpoint.stream,
        home.path().to_path_buf(),
    );
    let created = host
        .handle(req(10, "session/new", serde_json::json!({})))
        .await
        .unwrap();
    let sid = created["result"]["sessionId"].as_str().unwrap();

    let first = host
        .handle(req(
            11,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"remember this large result"}),
        ))
        .await
        .unwrap();
    assert_eq!(first["result"]["status"], "complete");
    let second = host
        .handle(req(
            12,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"continue now"}),
        ))
        .await
        .unwrap();
    assert_eq!(second["result"]["status"], "complete");

    let contexts = contexts.lock().unwrap();
    assert_eq!(contexts.len(), 3, "expected sample, compact, sample");
    assert!(contexts[1]["tools"].as_array().unwrap().is_empty());
    let rebuilt = contexts[2].to_string();
    assert!(rebuilt.contains("conversation_summary"));
    assert!(rebuilt.contains("continue now"));
    assert!(!rebuilt.contains("SECRET_AUTOMATIC_PRECOMPACTION_PAYLOAD"));
}

#[tokio::test]
async fn provider_overflow_compacts_and_resubmits_the_turn_exactly_once() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let overflow = || {
        ModelError::new(
            "model.context_overflow",
            "context window exceeded",
            Retryability::Never,
        )
        .with_kind(ModelErrorKind::ContextOverflow)
        .with_context_window(100_000)
    };
    let raw: Arc<dyn ModelStream> = Arc::new(RecoveryStream {
        scripts: tokio::sync::Mutex::new(vec![
            Ok(vec![StreamPiece::Text("seed history ".repeat(2_000))]),
            Err(overflow()),
            Ok(vec![StreamPiece::Text(summary())]),
            Ok(vec![StreamPiece::Text("recovered".into())]),
        ]),
        contexts: contexts.clone(),
    });
    let endpoint = adapt_model_endpoint(
        "fixture",
        "overflow-recovery",
        ModelMetadata {
            context_window: Some(1_000_000),
            model_family: Some("fixture".into()),
        },
        raw,
    )
    .unwrap();
    let (updates, _updates_rx) = mpsc::unbounded_channel();
    let mut host = AcpHost::new_with_home(
        workspace.path().to_path_buf(),
        SessionTrust::for_headless_prompt(workspace.path()),
        updates,
        endpoint.stream,
        home.path().to_path_buf(),
    );
    let created = host
        .handle(req(20, "session/new", serde_json::json!({})))
        .await
        .unwrap();
    let sid = created["result"]["sessionId"].as_str().unwrap();
    assert_eq!(
        host.handle(req(
            21,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"seed"}),
        ))
        .await
        .unwrap()["result"]["status"],
        "complete"
    );
    let recovered = host
        .handle(req(
            22,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"continue"}),
        ))
        .await
        .unwrap();
    assert_eq!(recovered["result"]["status"], "complete", "{recovered}");

    let contexts = contexts.lock().unwrap();
    assert_eq!(contexts.len(), 4, "seed, overflow, compact, resubmit");
    assert!(!contexts[1]["tools"].as_array().unwrap().is_empty());
    assert!(contexts[2]["tools"].as_array().unwrap().is_empty());
    assert!(!contexts[3]["tools"].as_array().unwrap().is_empty());
    assert!(contexts[3].to_string().contains("conversation_summary"));
}
