use async_trait::async_trait;
use lato_agent::{HistoryItem, RuntimePromptOutcome, RuntimeSession, default_fake_stream};
use lato_ai::{ModelStream, StreamPiece};
use lato_core::CancelReason;
use lato_workspace::{FileLocks, SessionTrust};
use std::sync::Arc;
use tokio::sync::{Notify, mpsc};
use tokio::time::{Duration, timeout};

fn session_with_stream(
    stream: Arc<dyn ModelStream>,
) -> (
    Arc<RuntimeSession>,
    mpsc::UnboundedReceiver<serde_json::Value>,
) {
    let cwd = std::env::current_dir().unwrap();
    let (updates_tx, updates_rx) = mpsc::unbounded_channel();
    let session = RuntimeSession::new(
        "runtime-session".into(),
        stream,
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(&cwd),
        cwd,
        updates_tx,
        None,
    );
    (Arc::new(session), updates_rx)
}

fn session() -> (
    Arc<RuntimeSession>,
    mpsc::UnboundedReceiver<serde_json::Value>,
) {
    session_with_stream(default_fake_stream())
}

#[tokio::test]
async fn prompt_round_trips_through_typed_runtime_events_exactly_once() {
    let (session, mut updates) = session();
    let outcome = session.prompt("hi".into()).await.unwrap();
    assert_eq!(
        outcome,
        RuntimePromptOutcome::Complete { text: "hi".into() }
    );

    let delta = timeout(Duration::from_secs(1), updates.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delta["method"], "session/update");
    assert_eq!(delta["params"]["delta"], "hi");
    assert!(updates.try_recv().is_err(), "delta was translated twice");
}

#[tokio::test]
async fn hydrated_history_is_visible_after_resume() {
    let (session, _) = session();
    session
        .replace_history(vec![HistoryItem::User("restored".into())])
        .await;
    let history = session.history_snapshot().await;
    assert!(matches!(&history[..], [HistoryItem::User(text)] if text == "restored"));
}

#[tokio::test]
async fn idle_cancel_is_idempotent() {
    let (session, _) = session();
    session.cancel().await.unwrap();
    session.cancel().await.unwrap();
}

struct BlockingStream {
    started: Notify,
}

#[async_trait]
impl ModelStream for BlockingStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        _context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), String> {
        self.started.notify_one();
        tx.closed().await;
        Ok(())
    }
}

#[tokio::test]
async fn concurrent_cancel_cannot_miss_a_submitted_turn() {
    let stream = Arc::new(BlockingStream {
        started: Notify::new(),
    });
    let (session, _) = session_with_stream(stream.clone());
    let prompt_session = session.clone();
    let prompt = tokio::spawn(async move { prompt_session.prompt("wait".into()).await });

    timeout(Duration::from_secs(1), stream.started.notified())
        .await
        .expect("prompt did not reach the model stream");
    session.cancel().await.unwrap();

    let outcome = timeout(Duration::from_secs(1), prompt)
        .await
        .expect("cancel did not terminate prompt")
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome,
        RuntimePromptOutcome::Cancelled {
            reason: CancelReason::User
        }
    );
    session.cancel().await.unwrap();
}

#[tokio::test]
async fn shutdown_stops_future_prompts_with_a_typed_error() {
    let (session, _) = session();
    session.shutdown().await.unwrap();
    let error = session.prompt("too late".into()).await.unwrap_err();
    assert_eq!(error.code, "runtime.command_bus_closed");
}
