use async_trait::async_trait;
use lato_agent::{AcpHost, REQUIRED_SECTIONS};
use lato_ai::{ModelCallReport, ModelMetadata, ModelStream, StreamPiece, adapt_model_endpoint};
use lato_core::{ModelError, ModelUsage, Retryability};
use lato_protocol::JsonRpcReq;
use lato_workspace::SessionTrust;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{Notify, mpsc};
use tokio::time::{Duration, timeout};

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

const NOTE1_SENTINEL: &str = "SECRET_SPECULATIVE_NOTE1";
const PREFIRE_SEED_TURNS: usize = 24;

struct PrefireLifecycleStream {
    ordinary_calls: AtomicUsize,
    compaction_calls: AtomicUsize,
    contexts: Mutex<Vec<serde_json::Value>>,
    pass_one_completed: Notify,
}

impl PrefireLifecycleStream {
    fn new() -> Self {
        Self {
            ordinary_calls: AtomicUsize::new(0),
            compaction_calls: AtomicUsize::new(0),
            contexts: Mutex::new(Vec::new()),
            pass_one_completed: Notify::new(),
        }
    }

    async fn respond(
        &self,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<ModelCallReport, ModelError> {
        self.contexts.lock().unwrap().push(context.clone());
        let is_compaction = context["tools"].as_array().is_some_and(Vec::is_empty);
        if is_compaction {
            let call = self.compaction_calls.fetch_add(1, Ordering::SeqCst);
            let text = match call {
                0 => {
                    self.pass_one_completed.notify_one();
                    NOTE1_SENTINEL.to_owned()
                }
                1 => summary(),
                _ => {
                    return Err(ModelError::new(
                        "test_fixture.unexpected_compaction",
                        "prefire lifecycle made more than pass one and pass two",
                        Retryability::Never,
                    ));
                }
            };
            tx.send(StreamPiece::Text(text))
                .await
                .map_err(|_| ModelError::cancelled())?;
            return Ok(ModelCallReport::default());
        }

        let call = self.ordinary_calls.fetch_add(1, Ordering::SeqCst);
        let usage = if call < PREFIRE_SEED_TURNS {
            tx.send(StreamPiece::Text(format!("seed-{call} ").repeat(20)))
                .await
                .map_err(|_| ModelError::cancelled())?;
            if call + 1 == PREFIRE_SEED_TURNS {
                750_000
            } else {
                10_000
            }
        } else if call == PREFIRE_SEED_TURNS {
            self.pass_one_completed.notified().await;
            tx.send(StreamPiece::ToolCall {
                id: "prefire-tool-84".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"missing-prefire-84"}),
            })
            .await
            .map_err(|_| ModelError::cancelled())?;
            840_000
        } else if call == PREFIRE_SEED_TURNS + 1 {
            tx.send(StreamPiece::ToolCall {
                id: "prefire-tool-85".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"missing-prefire-85"}),
            })
            .await
            .map_err(|_| ModelError::cancelled())?;
            850_000
        } else if call == PREFIRE_SEED_TURNS + 2 {
            tx.send(StreamPiece::Text("prefire lifecycle complete".into()))
                .await
                .map_err(|_| ModelError::cancelled())?;
            1_000
        } else {
            return Err(ModelError::new(
                "test_fixture.unexpected_ordinary",
                "prefire lifecycle made an unexpected ordinary request",
                Retryability::Never,
            ));
        };
        Ok(ModelCallReport {
            usage: Some(ModelUsage {
                input_tokens: Some(usage),
                output_tokens: Some(0),
                reasoning_tokens: None,
                cached_input_tokens: None,
            }),
            generation: 0,
        })
    }
}

#[async_trait]
impl ModelStream for PrefireLifecycleStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError> {
        self.respond(context, tx).await.map(|_| ())
    }

    async fn stream_with_report(
        &self,
        _prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<ModelCallReport, ModelError> {
        self.respond(context, tx).await
    }
}

#[tokio::test]
async fn prefire_runs_once_at_75_reuses_at_84_and_finalizes_at_85_without_leaking_note1() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let stream = Arc::new(PrefireLifecycleStream::new());
    let raw: Arc<dyn ModelStream> = stream.clone();
    let endpoint = adapt_model_endpoint(
        "fixture",
        "prefire-lifecycle",
        ModelMetadata {
            context_window: Some(1_000_000),
            model_family: Some("fixture".into()),
        },
        raw,
    )
    .unwrap();
    let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
    let mut host = AcpHost::new_with_home(
        workspace.path().to_path_buf(),
        SessionTrust::for_headless_prompt(workspace.path()),
        updates_tx,
        endpoint.stream,
        home.path().to_path_buf(),
    );
    let created = host
        .handle(req(20, "session/new", serde_json::json!({})))
        .await
        .unwrap();
    let sid = created["result"]["sessionId"].as_str().unwrap();
    for index in 0..PREFIRE_SEED_TURNS {
        let response = timeout(
            Duration::from_secs(2),
            host.handle(req(
                21 + index as i32,
                "session/prompt",
                serde_json::json!({"sessionId":sid,"text":format!("seed objective {index}")}),
            )),
        )
        .await
        .expect("seed prompt timed out")
        .unwrap();
        assert_eq!(response["result"]["status"], "complete", "{response}");
    }

    let response = timeout(
        Duration::from_secs(2),
        host.handle(req(
            100,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"cross prefire and final thresholds"}),
        )),
    )
    .await
    .expect("prefire/final lifecycle timed out")
    .unwrap();
    assert_eq!(response["result"]["status"], "complete", "{response}");
    assert_eq!(response["result"]["text"], "prefire lifecycle complete");
    let updates = std::iter::from_fn(|| updates_rx.try_recv().ok()).collect::<Vec<_>>();
    let utilization = updates
        .iter()
        .filter(|update| update["method"] == "lato/session/context")
        .filter_map(|update| update["params"]["utilizationPercent"].as_u64())
        .collect::<Vec<_>>();
    for boundary in [75, 84, 85] {
        assert!(
            utilization.contains(&boundary),
            "missing {boundary}% from utilization sequence {utilization:?}"
        );
    }
    assert_eq!(
        stream.compaction_calls.load(Ordering::SeqCst),
        2,
        "observed utilization sequence: {utilization:?}"
    );
    assert_eq!(
        stream.ordinary_calls.load(Ordering::SeqCst),
        PREFIRE_SEED_TURNS + 3
    );

    let contexts = stream.contexts.lock().unwrap();
    let compaction = contexts
        .iter()
        .filter(|context| context["tools"].as_array().is_some_and(Vec::is_empty))
        .collect::<Vec<_>>();
    assert_eq!(
        compaction.len(),
        2,
        "one pass-one request and one final pass-two request"
    );
    assert!(!compaction[0].to_string().contains(NOTE1_SENTINEL));
    assert!(compaction[1].to_string().contains(NOTE1_SENTINEL));
    assert_eq!(
        contexts
            .iter()
            .filter(|context| context.to_string().contains(NOTE1_SENTINEL))
            .count(),
        1,
        "NOTE1 must appear only in the pass-two model request"
    );
    drop(contexts);

    let encoded_updates = serde_json::to_string(&updates).unwrap();
    assert!(!encoded_updates.contains(NOTE1_SENTINEL));
    assert_eq!(
        updates
            .iter()
            .filter(|update| update["method"] == "session/update")
            .filter(|update| {
                update["params"]["delta"].as_str() == Some("prefire lifecycle complete")
            })
            .count(),
        1,
        "runtime ModelDelta must become exactly one ACP assistant delta"
    );

    let session_dir = home.path().join("sessions").join(sid);
    let journal = std::fs::read_to_string(session_dir.join("events.jsonl")).unwrap();
    let replacement = std::fs::read_to_string(session_dir.join("history.jsonl")).unwrap();
    assert!(journal.contains("conversation_item_committed"));
    assert!(!journal.contains(NOTE1_SENTINEL));
    assert!(replacement.contains("conversation_summary"));
    assert!(!replacement.contains(NOTE1_SENTINEL));
}
