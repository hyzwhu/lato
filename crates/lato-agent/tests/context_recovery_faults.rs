use async_trait::async_trait;
use lato_agent::{AcpHost, REQUIRED_SECTIONS};
use lato_ai::{ModelMetadata, ModelStream, StreamPiece, adapt_model_endpoint};
use lato_core::{ModelError, ModelErrorKind, Retryability};
use lato_protocol::JsonRpcReq;
use lato_workspace::SessionTrust;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::mpsc;

#[derive(Clone)]
struct FaultStep {
    pieces: Vec<StreamPiece>,
    terminal: Result<(), ModelError>,
}

struct FaultStream {
    steps: tokio::sync::Mutex<VecDeque<FaultStep>>,
    contexts: Mutex<Vec<serde_json::Value>>,
    calls: AtomicUsize,
}

impl FaultStream {
    fn new(steps: Vec<FaultStep>) -> Self {
        Self {
            steps: tokio::sync::Mutex::new(steps.into()),
            contexts: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn contexts(&self) -> Vec<serde_json::Value> {
        self.contexts.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModelStream for FaultStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.contexts.lock().unwrap().push(context);
        let Some(step) = self.steps.lock().await.pop_front() else {
            return Err(ModelError::new(
                "test_fixture.exhausted",
                "model request consumed more fault steps than provided",
                Retryability::Never,
            ));
        };
        for piece in step.pieces {
            tx.send(piece).await.map_err(|_| ModelError::cancelled())?;
        }
        step.terminal
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

fn success(pieces: Vec<StreamPiece>) -> FaultStep {
    FaultStep {
        pieces,
        terminal: Ok(()),
    }
}

fn failure(pieces: Vec<StreamPiece>, error: ModelError) -> FaultStep {
    FaultStep {
        pieces,
        terminal: Err(error),
    }
}

fn overflow(message: &str) -> ModelError {
    ModelError::new("model.context_overflow", message, Retryability::Never)
        .with_kind(ModelErrorKind::ContextOverflow)
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

fn make_host(
    workspace: &tempfile::TempDir,
    home: &tempfile::TempDir,
    stream: Arc<FaultStream>,
) -> (AcpHost, mpsc::UnboundedReceiver<serde_json::Value>) {
    let raw: Arc<dyn ModelStream> = stream;
    let endpoint = adapt_model_endpoint(
        "fixture",
        "context-recovery-faults",
        ModelMetadata {
            context_window: Some(1_000_000),
            model_family: Some("fixture".into()),
        },
        raw,
    )
    .unwrap();
    let (updates_tx, updates_rx) = mpsc::unbounded_channel();
    (
        AcpHost::new_with_home(
            workspace.path().to_path_buf(),
            SessionTrust::for_headless_prompt(workspace.path()),
            updates_tx,
            endpoint.stream,
            home.path().to_path_buf(),
        ),
        updates_rx,
    )
}

async fn new_session(host: &mut AcpHost) -> String {
    host.handle(req(1, "session/new", serde_json::json!({})))
        .await
        .unwrap()["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn seed(host: &mut AcpHost, sid: &str) {
    let response = host
        .handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"seed"}),
        ))
        .await
        .unwrap();
    assert_eq!(response["result"]["status"], "complete", "{response}");
}

fn deltas(updates: &mut mpsc::UnboundedReceiver<serde_json::Value>) -> Vec<String> {
    std::iter::from_fn(|| updates.try_recv().ok())
        .filter(|update| update["method"] == "session/update")
        .filter_map(|update| update["params"]["delta"].as_str().map(str::to_owned))
        .collect()
}

fn terminal_message(response: &serde_json::Value) -> &str {
    response["error"]["message"].as_str().unwrap()
}

#[tokio::test]
async fn first_pre_output_overflow_compacts_and_resubmits_exactly_once() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![
        success(vec![StreamPiece::Text("seed history ".repeat(2_000))]),
        failure(Vec::new(), overflow("first overflow")),
        success(vec![StreamPiece::Text(summary())]),
        success(vec![StreamPiece::Text("recovered".into())]),
    ]));
    let (mut host, mut updates) = make_host(&workspace, &home, stream.clone());
    let sid = new_session(&mut host).await;
    seed(&mut host, &sid).await;

    let response = host
        .handle(req(
            3,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"continue"}),
        ))
        .await
        .unwrap();

    assert_eq!(response["result"]["status"], "complete", "{response}");
    assert_eq!(response["result"]["text"], "recovered");
    assert_eq!(stream.calls(), 4);
    let contexts = stream.contexts();
    assert_eq!(contexts.len(), 4, "seed, overflow, compact, resubmit");
    assert!(!contexts[1]["tools"].as_array().unwrap().is_empty());
    assert!(contexts[2]["tools"].as_array().unwrap().is_empty());
    assert!(!contexts[3]["tools"].as_array().unwrap().is_empty());
    assert!(contexts[3].to_string().contains("conversation_summary"));
    let visible = deltas(&mut updates).join("");
    assert!(visible.ends_with("recovered"));
    assert_eq!(visible.matches("recovered").count(), 1);
    assert!(!visible.contains("first overflow"));
}

#[tokio::test]
async fn second_overflow_is_terminal_without_another_recovery_or_submission() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![
        success(vec![StreamPiece::Text("seed history ".repeat(2_000))]),
        failure(Vec::new(), overflow("first overflow")),
        success(vec![StreamPiece::Text(summary())]),
        failure(Vec::new(), overflow("second overflow")),
    ]));
    let (mut host, _) = make_host(&workspace, &home, stream.clone());
    let sid = new_session(&mut host).await;
    seed(&mut host, &sid).await;

    let response = host
        .handle(req(
            3,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"continue"}),
        ))
        .await
        .unwrap();

    assert_eq!(
        terminal_message(&response),
        "legacy.turn_failed: model.context_overflow: second overflow"
    );
    assert_eq!(
        stream.calls(),
        4,
        "must not consume a nonexistent fifth step"
    );
}

#[tokio::test]
async fn text_before_overflow_is_visible_once_and_never_replayed() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![failure(
        vec![StreamPiece::Text("partial".into())],
        overflow("overflow after text"),
    )]));
    let (mut host, mut updates) = make_host(&workspace, &home, stream.clone());
    let sid = new_session(&mut host).await;

    let response = host
        .handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"produce text"}),
        ))
        .await
        .unwrap();

    assert_eq!(
        terminal_message(&response),
        "legacy.turn_failed: model.context_overflow: overflow after text"
    );
    assert_eq!(stream.calls(), 1);
    assert_eq!(deltas(&mut updates), vec!["partial"]);
}

#[tokio::test]
async fn tool_call_before_overflow_is_never_replayed() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![failure(
        vec![StreamPiece::ToolCall {
            id: "fault-call-1".into(),
            name: "definitely_missing_test_tool".into(),
            arguments: serde_json::json!({}),
        }],
        overflow("overflow after tool call"),
    )]));
    let (mut host, _) = make_host(&workspace, &home, stream.clone());
    let sid = new_session(&mut host).await;

    let response = host
        .handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"call a tool"}),
        ))
        .await
        .unwrap();

    assert_eq!(
        terminal_message(&response),
        "legacy.turn_failed: model.context_overflow: overflow after tool call"
    );
    assert_eq!(stream.calls(), 1);
}

#[tokio::test]
async fn unchanged_recovery_keeps_the_provider_overflow_as_primary_failure() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![
        success(vec![StreamPiece::Text("seed history ".repeat(2_000))]),
        failure(Vec::new(), overflow("primary provider failure")),
        success(vec![StreamPiece::Text("invalid summary one".into())]),
        success(vec![StreamPiece::Text("invalid summary two".into())]),
        success(vec![StreamPiece::Text("invalid summary three".into())]),
    ]));
    let (mut host, _) = make_host(&workspace, &home, stream.clone());
    let sid = new_session(&mut host).await;
    seed(&mut host, &sid).await;

    let response = host
        .handle(req(
            3,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"continue"}),
        ))
        .await
        .unwrap();

    assert_eq!(
        terminal_message(&response),
        "legacy.turn_failed: model.context_overflow: primary provider failure"
    );
    assert_eq!(stream.calls(), 5);
}

#[tokio::test]
async fn fatal_recovery_appends_context_without_replacing_the_provider_failure() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let stream = Arc::new(FaultStream::new(vec![
        success(vec![StreamPiece::Text("seed history ".repeat(2_000))]),
        failure(Vec::new(), overflow("primary provider failure")),
        failure(
            Vec::new(),
            ModelError::new(
                "model.auth",
                "compaction credentials expired",
                Retryability::Never,
            )
            .with_kind(ModelErrorKind::Authentication),
        ),
    ]));
    let (mut host, _) = make_host(&workspace, &home, stream.clone());
    let sid = new_session(&mut host).await;
    seed(&mut host, &sid).await;

    let response = host
        .handle(req(
            3,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"continue"}),
        ))
        .await
        .unwrap();

    let message = terminal_message(&response);
    assert!(
        message.starts_with("legacy.turn_failed: model.context_overflow: primary provider failure"),
        "{message}"
    );
    assert!(
        message.contains(
            "context recovery failed: compaction.model_failed: compaction model failed: \
             model.auth: compaction credentials expired"
        ),
        "{message}"
    );
    assert_eq!(stream.calls(), 3);
}
