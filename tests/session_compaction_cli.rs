use async_trait::async_trait;
use lato_agent::{AcpHost, REQUIRED_SECTIONS};
use lato_ai::{ModelCallReport, ModelMetadata, ModelStream, StreamPiece, adapt_model_endpoint};
use lato_core::{ModelError, ModelErrorKind, ModelUsage, Retryability};
use lato_protocol::JsonRpcReq;
use lato_workspace::SessionTrust;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    sync::{Notify, mpsc},
    time::{Duration, timeout},
};

const RAW_PAYLOAD_SENTINEL: &str = "SECRET_AUTOMATIC_RECOVERY_PAYLOAD";
const NOTE1_SENTINEL: &str = "SECRET_SPECULATIVE_NOTE1";

struct ScriptStep {
    expected: ExpectedCallKind,
    pieces: Vec<StreamPiece>,
    terminal: Result<(), ModelError>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpectedCallKind {
    Ordinary,
    Compaction,
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
        self.contexts.lock().unwrap().push(context.clone());
        let Some(script) = self.scripts.lock().await.pop_front() else {
            return Err(ModelError::new(
                "test_fixture.exhausted",
                "model request consumed more scripted steps than provided",
                Retryability::Never,
            ));
        };
        let actual = if context["tools"].as_array().is_some_and(Vec::is_empty) {
            ExpectedCallKind::Compaction
        } else {
            ExpectedCallKind::Ordinary
        };
        assert_eq!(
            actual, script.expected,
            "unexpected model request: {context}"
        );
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

fn ordinary_success(pieces: Vec<StreamPiece>) -> ScriptStep {
    ScriptStep {
        expected: ExpectedCallKind::Ordinary,
        pieces,
        terminal: Ok(()),
    }
}

fn ordinary_failure(pieces: Vec<StreamPiece>, error: ModelError) -> ScriptStep {
    ScriptStep {
        expected: ExpectedCallKind::Ordinary,
        pieces,
        terminal: Err(error),
    }
}

fn compaction_success(pieces: Vec<StreamPiece>) -> ScriptStep {
    ScriptStep {
        expected: ExpectedCallKind::Compaction,
        pieces,
        terminal: Ok(()),
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
    // Windows CI runners exhibit multi-second fsync latency spikes; a request
    // that legitimately completes in seconds must not fail the fixture, while
    // real deadlocks still time out well below CI job limits.
    timeout(Duration::from_secs(30), host.handle(request))
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

fn assert_closed(response: &serde_json::Value) {
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["closed"], true, "{response}");
}

fn assert_scoped_updates(updates: &[serde_json::Value], sid: &str) {
    for update in updates {
        let method = update["method"].as_str().unwrap_or_default();
        if method.starts_with("session/") || method.starts_with("lato/session/") {
            assert_eq!(update["params"]["sessionId"], sid, "{update}");
        }
    }
}

fn assistant_updates(
    updates: &[serde_json::Value],
    sid: &str,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let scoped = updates.iter().filter(|update| {
        update["method"] == "session/update" && update["params"]["sessionId"] == sid
    });
    let deltas = scoped
        .clone()
        .filter_map(|update| update["params"]["delta"].as_str().map(str::to_owned))
        .collect();
    let texts = scoped
        .filter_map(|update| update["params"]["text"].as_str().map(str::to_owned))
        .collect();
    let reasoning = updates
        .iter()
        .filter(|update| {
            update["method"] == "session/reasoning" && update["params"]["sessionId"] == sid
        })
        .filter_map(|update| update["params"]["delta"].as_str().map(str::to_owned))
        .collect();
    (deltas, texts, reasoning)
}

fn assert_one_compaction_lifecycle(updates: &[serde_json::Value], sid: &str, trigger: &str) {
    assert_scoped_updates(updates, sid);
    let compaction = updates
        .iter()
        .filter(|update| {
            update["method"] == "lato/session/compaction" && update["params"]["sessionId"] == sid
        })
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

fn assert_resumed_context(context: &serde_json::Value, raw_sentinel: &str, new_user_input: &str) {
    let messages = context["messages"].as_array().unwrap();
    let content = messages
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        content
            .iter()
            .filter(|text| text.contains("<conversation_summary version=\"1\">"))
            .count(),
        1,
        "{messages:?}"
    );
    let summary_index = messages
        .iter()
        .position(|message| {
            message["content"]
                .as_str()
                .is_some_and(|text| text.contains("<conversation_summary version=\"1\">"))
        })
        .unwrap();
    assert_eq!(messages[summary_index]["role"], "user");
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"], new_user_input);
    assert!(summary_index < messages.len() - 1);
    let encoded = serde_json::to_string(messages).unwrap();
    assert!(!encoded.contains(raw_sentinel));
    assert!(!encoded.contains(NOTE1_SENTINEL));
    assert_eq!(
        content
            .iter()
            .map(|text| text.matches(new_user_input).count())
            .sum::<usize>(),
        1
    );
}

const PREFIRE_SEED_TURNS: usize = 24;

struct PrefireRestartStream {
    ordinary_calls: AtomicUsize,
    compaction_calls: AtomicUsize,
    contexts: Mutex<Vec<serde_json::Value>>,
    pass_one_completed: Notify,
}

impl PrefireRestartStream {
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
        if context["tools"].as_array().is_some_and(Vec::is_empty) {
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
                        "prefire restart made more than pass one and pass two",
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
            let marker = if call == 0 {
                RAW_PAYLOAD_SENTINEL
            } else {
                "seed"
            };
            tx.send(StreamPiece::Text(format!("{marker}-{call} ").repeat(20)))
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
                id: "prefire-restart-84".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"missing-prefire-restart-84"}),
            })
            .await
            .map_err(|_| ModelError::cancelled())?;
            840_000
        } else if call == PREFIRE_SEED_TURNS + 1 {
            tx.send(StreamPiece::ToolCall {
                id: "prefire-restart-85".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"missing-prefire-restart-85"}),
            })
            .await
            .map_err(|_| ModelError::cancelled())?;
            850_000
        } else if call == PREFIRE_SEED_TURNS + 2 {
            tx.send(StreamPiece::Text("prefire checkpoint complete".into()))
                .await
                .map_err(|_| ModelError::cancelled())?;
            1_000
        } else {
            return Err(ModelError::new(
                "test_fixture.unexpected_ordinary",
                "prefire restart made an unexpected ordinary request",
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
impl ModelStream for PrefireRestartStream {
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
async fn provider_overflow_recovery_is_exactly_once_and_survives_restart() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let trust = SessionTrust::for_headless_prompt(workspace.path());
    let first_contexts = Arc::new(Mutex::new(Vec::new()));
    let first_stream: Arc<dyn ModelStream> = Arc::new(RecordingStream {
        scripts: tokio::sync::Mutex::new(
            vec![
                ordinary_success(vec![StreamPiece::Text(format!(
                    "{RAW_PAYLOAD_SENTINEL} {}",
                    "preserve raw context ".repeat(2_000)
                ))]),
                ordinary_failure(Vec::new(), overflow("first overflow")),
                compaction_success(vec![StreamPiece::Text(summary())]),
                ordinary_success(vec![StreamPiece::Text("continued before restart".into())]),
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

    let mut recovery_updates = drain_updates(&mut updates_rx);
    let closed = handle(
        &mut first,
        req(4, "session/close", serde_json::json!({"sessionId":sid})),
    )
    .await;
    assert_closed(&closed);
    recovery_updates.extend(drain_updates(&mut updates_rx));
    assert_one_compaction_lifecycle(&recovery_updates, &sid, "provider_overflow");
    let (deltas, texts, reasoning) = assistant_updates(&recovery_updates, &sid);
    assert_eq!(deltas, vec!["continued before restart"]);
    assert_eq!(texts, vec!["continued before restart"]);
    assert!(reasoning.is_empty(), "{recovery_updates:?}");

    let contexts = first_contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 4, "seed, rejected, compaction, resubmit");
    let rebuilt = contexts[3].to_string();
    assert!(rebuilt.contains("conversation_summary"));
    assert!(!rebuilt.contains(RAW_PAYLOAD_SENTINEL));
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
            vec![ordinary_success(vec![StreamPiece::Text(
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
    let mut resumed_updates = drain_updates(&mut resumed_updates_rx);
    let closed = handle(
        &mut resumed,
        req(7, "session/close", serde_json::json!({"sessionId":sid})),
    )
    .await;
    assert_closed(&closed);
    resumed_updates.extend(drain_updates(&mut resumed_updates_rx));
    assert_scoped_updates(&resumed_updates, &sid);
    let (deltas, texts, reasoning) = assistant_updates(&resumed_updates, &sid);
    assert_eq!(deltas, vec!["continued after restart"]);
    assert_eq!(texts, vec!["continued after restart"]);
    assert!(reasoning.is_empty(), "{resumed_updates:?}");
    let contexts = resumed_contexts.lock().unwrap();
    assert_eq!(contexts.len(), 1);
    assert_resumed_context(&contexts[0], RAW_PAYLOAD_SENTINEL, "resume the task");
}

#[tokio::test]
async fn speculative_prefire_note_is_used_by_pass_two_but_never_survives_restart() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let trust = SessionTrust::for_headless_prompt(workspace.path());
    let stream = Arc::new(PrefireRestartStream::new());
    let raw: Arc<dyn ModelStream> = stream.clone();
    let (updates, mut updates_rx) = mpsc::unbounded_channel();
    let mut first = AcpHost::new_with_home(
        workspace.path().to_path_buf(),
        trust.clone(),
        updates,
        endpoint(raw),
        home.path().to_path_buf(),
    );
    let created = handle(&mut first, req(20, "session/new", serde_json::json!({}))).await;
    let sid = created["result"]["sessionId"].as_str().unwrap().to_owned();
    for index in 0..PREFIRE_SEED_TURNS {
        let response = handle(
            &mut first,
            req(
                21 + index as i32,
                "session/prompt",
                serde_json::json!({"sessionId":sid,"text":format!("seed objective {index}")}),
            ),
        )
        .await;
        assert_eq!(response["result"]["status"], "complete", "{response}");
    }
    drain_updates(&mut updates_rx);

    let response = handle(
        &mut first,
        req(
            100,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"cross prefire and final thresholds"}),
        ),
    )
    .await;
    assert_eq!(response["result"]["status"], "complete", "{response}");
    assert_eq!(response["result"]["text"], "prefire checkpoint complete");
    let mut updates = drain_updates(&mut updates_rx);
    let closed = handle(
        &mut first,
        req(101, "session/close", serde_json::json!({"sessionId":sid})),
    )
    .await;
    assert_closed(&closed);
    updates.extend(drain_updates(&mut updates_rx));
    assert_one_compaction_lifecycle(&updates, &sid, "threshold");
    let (deltas, texts, reasoning) = assistant_updates(&updates, &sid);
    assert_eq!(deltas, vec!["prefire checkpoint complete"]);
    assert_eq!(texts, vec!["prefire checkpoint complete"]);
    assert!(reasoning.is_empty(), "{updates:?}");
    assert!(
        !serde_json::to_string(&updates)
            .unwrap()
            .contains(NOTE1_SENTINEL)
    );
    drop(first);

    assert_eq!(stream.compaction_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        stream.ordinary_calls.load(Ordering::SeqCst),
        PREFIRE_SEED_TURNS + 3
    );
    let contexts = stream.contexts.lock().unwrap().clone();
    let compaction = contexts
        .iter()
        .filter(|context| context["tools"].as_array().is_some_and(Vec::is_empty))
        .collect::<Vec<_>>();
    assert_eq!(compaction.len(), 2);
    assert!(!compaction[0].to_string().contains(NOTE1_SENTINEL));
    assert!(compaction[1].to_string().contains(NOTE1_SENTINEL));
    assert_eq!(
        contexts
            .iter()
            .filter(|context| context.to_string().contains(NOTE1_SENTINEL))
            .count(),
        1
    );

    let session_dir = home.path().join("sessions").join(&sid);
    let journal = std::fs::read_to_string(session_dir.join("events.jsonl")).unwrap();
    let history = std::fs::read_to_string(session_dir.join("history.jsonl")).unwrap();
    assert!(!journal.contains(NOTE1_SENTINEL));
    assert!(!history.contains(NOTE1_SENTINEL));
    assert!(!history.contains(RAW_PAYLOAD_SENTINEL));
    assert!(history.contains("conversation_summary"));

    let resumed_contexts = Arc::new(Mutex::new(Vec::new()));
    let resumed_stream: Arc<dyn ModelStream> = Arc::new(RecordingStream {
        scripts: tokio::sync::Mutex::new(
            vec![ordinary_success(vec![StreamPiece::Text(
                "prefire resumed".into(),
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
        req(102, "session/resume", serde_json::json!({"sessionId":sid})),
    )
    .await;
    assert!(response.get("error").is_none(), "{response}");
    let response = handle(
        &mut resumed,
        req(
            103,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"resume after prefire"}),
        ),
    )
    .await;
    assert_eq!(response["result"]["status"], "complete", "{response}");
    assert_eq!(response["result"]["text"], "prefire resumed");
    let mut updates = drain_updates(&mut resumed_updates_rx);
    let closed = handle(
        &mut resumed,
        req(104, "session/close", serde_json::json!({"sessionId":sid})),
    )
    .await;
    assert_closed(&closed);
    updates.extend(drain_updates(&mut resumed_updates_rx));
    assert_scoped_updates(&updates, &sid);
    let (deltas, texts, reasoning) = assistant_updates(&updates, &sid);
    assert_eq!(deltas, vec!["prefire resumed"]);
    assert_eq!(texts, vec!["prefire resumed"]);
    assert!(reasoning.is_empty(), "{updates:?}");
    let contexts = resumed_contexts.lock().unwrap();
    assert_eq!(contexts.len(), 1);
    assert_resumed_context(&contexts[0], RAW_PAYLOAD_SENTINEL, "resume after prefire");
}

#[tokio::test]
async fn second_overflow_returns_one_error_without_another_submission() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let stream: Arc<dyn ModelStream> = Arc::new(RecordingStream {
        scripts: tokio::sync::Mutex::new(
            vec![
                ordinary_success(vec![StreamPiece::Text("seed history ".repeat(2_000))]),
                ordinary_failure(Vec::new(), overflow("first overflow")),
                compaction_success(vec![StreamPiece::Text(summary())]),
                ordinary_failure(Vec::new(), overflow("second overflow")),
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
    let mut terminal_updates = drain_updates(&mut updates_rx);
    let closed = handle(
        &mut host,
        req(4, "session/close", serde_json::json!({"sessionId":sid})),
    )
    .await;
    assert_closed(&closed);
    terminal_updates.extend(drain_updates(&mut updates_rx));
    assert_one_compaction_lifecycle(&terminal_updates, sid, "provider_overflow");
    let (deltas, texts, reasoning) = assistant_updates(&terminal_updates, sid);
    assert!(deltas.is_empty(), "{terminal_updates:?}");
    assert!(texts.is_empty(), "{terminal_updates:?}");
    assert!(reasoning.is_empty(), "{terminal_updates:?}");
    assert_eq!(contexts.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn post_output_overflow_returns_one_error_and_never_replays_partial_delta() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let stream: Arc<dyn ModelStream> = Arc::new(RecordingStream {
        scripts: tokio::sync::Mutex::new(
            vec![ordinary_failure(
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
    let mut terminal_updates = drain_updates(&mut updates_rx);
    let closed = handle(
        &mut host,
        req(3, "session/close", serde_json::json!({"sessionId":sid})),
    )
    .await;
    assert_closed(&closed);
    terminal_updates.extend(drain_updates(&mut updates_rx));
    assert_scoped_updates(&terminal_updates, sid);
    let (deltas, texts, reasoning) = assistant_updates(&terminal_updates, sid);
    assert_eq!(deltas, vec!["partial"], "{terminal_updates:?}");
    assert!(texts.is_empty(), "{terminal_updates:?}");
    assert!(reasoning.is_empty(), "{terminal_updates:?}");
    assert!(
        terminal_updates
            .iter()
            .all(|update| update["method"] != "lato/session/compaction")
    );
    assert_eq!(contexts.lock().unwrap().len(), 1);
}
