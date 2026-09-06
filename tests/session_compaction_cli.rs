use async_trait::async_trait;
use lato_agent::{AcpHost, REQUIRED_SECTIONS};
use lato_ai::{ModelMetadata, ModelStream, StreamPiece, adapt_model_endpoint};
use lato_core::{ModelError, ModelErrorKind, Retryability};
use lato_protocol::JsonRpcReq;
use lato_workspace::SessionTrust;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::{
    sync::mpsc,
    time::{Duration, timeout},
};

const RAW_PAYLOAD_SENTINEL: &str = "SECRET_AUTOMATIC_RECOVERY_PAYLOAD";
const NOTE1_SENTINEL: &str = "SECRET_SPECULATIVE_NOTE1";

struct ScriptStep {
    pieces: Vec<StreamPiece>,
    terminal: Result<(), ModelError>,
}

struct RecordingStream {
    scripts: tokio::sync::Mutex<VecDeque<ScriptStep>>,
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
        let Some(script) = self.scripts.lock().await.pop_front() else {
            return Err(ModelError::new(
                "test_fixture.exhausted",
                "model request consumed more scripted steps than provided",
                Retryability::Never,
            ));
        };
        for piece in script.pieces {
            tx.send(piece).await.map_err(|_| {
                ModelError::new(
                    "model.receiver_closed",
                    "model stream receiver closed",
                    Retryability::Never,
                )
            })?;
        }
        script.terminal
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

fn success(pieces: Vec<StreamPiece>) -> ScriptStep {
    ScriptStep {
        pieces,
        terminal: Ok(()),
    }
}

fn failure(pieces: Vec<StreamPiece>, error: ModelError) -> ScriptStep {
    ScriptStep {
        pieces,
        terminal: Err(error),
    }
}

fn overflow(message: &str) -> ModelError {
    ModelError::new("model.context_overflow", message, Retryability::Never)
        .with_kind(ModelErrorKind::ContextOverflow)
        .with_status(400)
        .with_context_window(100_000)
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
            context_window: Some(1_000_000),
            model_family: Some("fixture".into()),
        },
        stream,
    )
    .unwrap()
    .stream
}

async fn handle(host: &mut AcpHost, request: JsonRpcReq) -> serde_json::Value {
    timeout(Duration::from_secs(1), host.handle(request))
        .await
        .expect("ACP request timed out")
        .expect("ACP request returned no response")
}

fn drain_updates(
    updates: &mut mpsc::UnboundedReceiver<serde_json::Value>,
) -> Vec<serde_json::Value> {
    std::iter::from_fn(|| updates.try_recv().ok()).collect()
}

fn assert_error_only(response: &serde_json::Value, message: &str) {
    assert!(response.get("result").is_none(), "{response}");
    assert_eq!(response["error"]["code"], -32000, "{response}");
    assert_eq!(response["error"]["message"], message, "{response}");
}

fn assert_one_compaction_lifecycle(updates: &[serde_json::Value], trigger: &str) {
    let compaction = updates
        .iter()
        .filter(|update| update["method"] == "lato/session/compaction")
        .collect::<Vec<_>>();
    assert_eq!(
        compaction
            .iter()
            .filter(|update| update["params"]["event"] == "started")
            .count(),
        1,
        "{compaction:?}"
    );
    assert!(compaction.iter().any(|update| {
        update["params"]["event"] == "started" && update["params"]["trigger"] == trigger
    }));
    let terminal = compaction
        .iter()
        .filter(|update| {
            matches!(
                update["params"]["event"].as_str(),
                Some("completed" | "failed" | "cancelled")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(terminal.len(), 1, "{compaction:?}");
    assert_eq!(terminal[0]["params"]["event"], "completed");
}

#[tokio::test]
async fn provider_overflow_recovery_is_exactly_once_and_survives_restart() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let trust = SessionTrust::for_headless_prompt(workspace.path());
    let first_contexts = Arc::new(Mutex::new(Vec::new()));
    let first_stream: Arc<dyn ModelStream> = Arc::new(RecordingStream {
        scripts: tokio::sync::Mutex::new(
            vec![
                success(vec![StreamPiece::Text(format!(
                    "{RAW_PAYLOAD_SENTINEL} {}",
                    "preserve raw context ".repeat(2_000)
                ))]),
                failure(Vec::new(), overflow("first overflow")),
                success(vec![StreamPiece::Text(summary())]),
                success(vec![StreamPiece::Text("continued before restart".into())]),
            ]
            .into(),
        ),
        contexts: first_contexts.clone(),
    });
    let (updates, mut updates_rx) = mpsc::unbounded_channel();
    let mut first = AcpHost::new_with_home(
        workspace.path().to_path_buf(),
        trust.clone(),
        updates,
        endpoint(first_stream),
        home.path().to_path_buf(),
    );
    let created = handle(&mut first, req(1, "session/new", serde_json::json!({}))).await;
    let sid = created["result"]["sessionId"].as_str().unwrap().to_owned();
    let seeded = handle(
        &mut first,
        req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"retain this work"}),
        ),
    )
    .await;
    assert_eq!(seeded["result"]["status"], "complete", "{seeded}");
    drain_updates(&mut updates_rx);

    let recovered = handle(
        &mut first,
        req(
            3,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"continue now"}),
        ),
    )
    .await;
    assert!(recovered.get("error").is_none(), "{recovered}");
    assert_eq!(recovered["result"]["status"], "complete");
    assert_eq!(recovered["result"]["text"], "continued before restart");

    let recovery_updates = drain_updates(&mut updates_rx);
    assert_one_compaction_lifecycle(&recovery_updates, "provider_overflow");
    let recovered_deltas = recovery_updates
        .iter()
        .filter_map(|update| update.pointer("/params/delta").and_then(|v| v.as_str()))
        .filter(|delta| *delta == "continued before restart")
        .count();
    let recovered_answers = recovery_updates
        .iter()
        .filter_map(|update| update.pointer("/params/text").and_then(|v| v.as_str()))
        .filter(|text| *text == "continued before restart")
        .count();
    assert_eq!(recovered_deltas, 1, "{recovery_updates:?}");
    assert_eq!(recovered_answers, 1, "{recovery_updates:?}");

    let contexts = first_contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 4, "seed, rejected, compaction, resubmit");
    assert!(!contexts[1]["tools"].as_array().unwrap().is_empty());
    assert!(contexts[2]["tools"].as_array().unwrap().is_empty());
    assert!(!contexts[3]["tools"].as_array().unwrap().is_empty());
    let rebuilt = contexts[3].to_string();
    assert!(rebuilt.contains("conversation_summary"));
    assert!(!rebuilt.contains(RAW_PAYLOAD_SENTINEL));

    handle(
        &mut first,
        req(4, "session/close", serde_json::json!({"sessionId":sid})),
    )
    .await;
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
    let journal = serde_json::to_string(&records).unwrap();
    assert!(!journal.contains(NOTE1_SENTINEL));

    let resumed_contexts = Arc::new(Mutex::new(Vec::new()));
    let resumed_stream: Arc<dyn ModelStream> = Arc::new(RecordingStream {
        scripts: tokio::sync::Mutex::new(
            vec![success(vec![StreamPiece::Text(
                "continued after restart".into(),
            )])]
            .into(),
        ),
        contexts: resumed_contexts.clone(),
    });
    let (updates, mut resumed_updates_rx) = mpsc::unbounded_channel();
    let mut resumed = AcpHost::new_with_home(
        workspace.path().to_path_buf(),
        trust,
        updates,
        endpoint(resumed_stream),
        home.path().to_path_buf(),
    );
    let response = handle(
        &mut resumed,
        req(5, "session/resume", serde_json::json!({"sessionId":sid})),
    )
    .await;
    assert!(response.get("error").is_none(), "{response}");
    let response = handle(
        &mut resumed,
        req(
            6,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"resume the task"}),
        ),
    )
    .await;
    assert_eq!(response["result"]["status"], "complete", "{response}");
    assert_eq!(response["result"]["text"], "continued after restart");
    let resumed_updates = drain_updates(&mut resumed_updates_rx);
    assert_eq!(
        resumed_updates
            .iter()
            .filter_map(|update| update.pointer("/params/delta").and_then(|v| v.as_str()))
            .filter(|delta| *delta == "continued after restart")
            .count(),
        1,
        "{resumed_updates:?}"
    );
    let contexts = resumed_contexts.lock().unwrap();
    assert_eq!(contexts.len(), 1);
    let encoded = contexts[0].to_string();
    assert!(encoded.contains("<conversation_summary version=\\\"1\\\">"));
    assert!(!encoded.contains(RAW_PAYLOAD_SENTINEL));
    assert!(!encoded.contains(NOTE1_SENTINEL));
    assert_eq!(encoded.matches("resume the task").count(), 1);
}

#[tokio::test]
async fn second_overflow_returns_one_error_without_another_submission() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let stream: Arc<dyn ModelStream> = Arc::new(RecordingStream {
        scripts: tokio::sync::Mutex::new(
            vec![
                success(vec![StreamPiece::Text("seed history ".repeat(2_000))]),
                failure(Vec::new(), overflow("first overflow")),
                success(vec![StreamPiece::Text(summary())]),
                failure(Vec::new(), overflow("second overflow")),
            ]
            .into(),
        ),
        contexts: contexts.clone(),
    });
    let (updates, mut updates_rx) = mpsc::unbounded_channel();
    let mut host = AcpHost::new_with_home(
        workspace.path().to_path_buf(),
        SessionTrust::for_headless_prompt(workspace.path()),
        updates,
        endpoint(stream),
        home.path().to_path_buf(),
    );
    let created = handle(&mut host, req(1, "session/new", serde_json::json!({}))).await;
    let sid = created["result"]["sessionId"].as_str().unwrap();
    let seeded = handle(
        &mut host,
        req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"seed"}),
        ),
    )
    .await;
    assert_eq!(seeded["result"]["status"], "complete", "{seeded}");
    drain_updates(&mut updates_rx);

    let response = handle(
        &mut host,
        req(
            3,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"continue"}),
        ),
    )
    .await;
    assert_error_only(
        &response,
        "legacy.turn_failed: model.context_overflow: second overflow",
    );
    let terminal_updates = drain_updates(&mut updates_rx);
    assert_one_compaction_lifecycle(&terminal_updates, "provider_overflow");
    assert!(
        terminal_updates
            .iter()
            .all(|update| update.pointer("/params/delta").is_none()
                && update.pointer("/params/text").is_none()),
        "{terminal_updates:?}"
    );
    assert_eq!(contexts.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn post_output_overflow_returns_one_error_and_never_replays_partial_delta() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let stream: Arc<dyn ModelStream> = Arc::new(RecordingStream {
        scripts: tokio::sync::Mutex::new(
            vec![failure(
                vec![StreamPiece::Text("partial".into())],
                overflow("overflow after text"),
            )]
            .into(),
        ),
        contexts: contexts.clone(),
    });
    let (updates, mut updates_rx) = mpsc::unbounded_channel();
    let mut host = AcpHost::new_with_home(
        workspace.path().to_path_buf(),
        SessionTrust::for_headless_prompt(workspace.path()),
        updates,
        endpoint(stream),
        home.path().to_path_buf(),
    );
    let created = handle(&mut host, req(1, "session/new", serde_json::json!({}))).await;
    let sid = created["result"]["sessionId"].as_str().unwrap();

    let response = handle(
        &mut host,
        req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"produce text"}),
        ),
    )
    .await;
    assert_error_only(
        &response,
        "legacy.turn_failed: model.context_overflow: overflow after text",
    );
    let terminal_updates = drain_updates(&mut updates_rx);
    let deltas = terminal_updates
        .iter()
        .filter_map(|update| update.pointer("/params/delta").and_then(|v| v.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(deltas, vec!["partial"], "{terminal_updates:?}");
    assert!(
        terminal_updates
            .iter()
            .all(|update| update["method"] != "lato/session/compaction"
                && update.pointer("/params/text").is_none()),
        "{terminal_updates:?}"
    );
    assert_eq!(contexts.lock().unwrap().len(), 1);
}
