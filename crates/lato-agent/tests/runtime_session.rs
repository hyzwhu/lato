use async_trait::async_trait;
use lato_agent::{
    HistoryItem, PreparedModelSwitch, REQUIRED_SECTIONS, RuntimePromptOutcome, RuntimeSession,
    default_fake_stream,
};
use lato_ai::{FakeModelStream, ModelMetadata, ModelStream, StreamPiece, adapt_model_endpoint};
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

    let delta = timeout(Duration::from_secs(1), async {
        loop {
            let update = updates.recv().await.unwrap();
            if update["method"] == "session/update" {
                break update;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(delta["method"], "session/update");
    assert_eq!(delta["params"]["delta"], "hi");
    assert!(
        std::iter::from_fn(|| updates.try_recv().ok())
            .all(|update| update["method"] != "session/update"),
        "delta was translated twice"
    );
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

fn endpoint(
    model: &str,
    family: &str,
    window: u64,
    scripts: Vec<Vec<StreamPiece>>,
) -> lato_ai::ActiveModelStream {
    adapt_model_endpoint(
        "fixture",
        model,
        ModelMetadata {
            context_window: Some(window),
            model_family: Some(family.into()),
        },
        Arc::new(FakeModelStream::new(scripts)),
    )
    .unwrap()
}

fn healthy_summary() -> String {
    let detail = "preserve verified decisions, evidence, and pending work ".repeat(2);
    REQUIRED_SECTIONS
        .iter()
        .enumerate()
        .map(|(index, heading)| format!("{}. {}: {detail}", index + 1, heading))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[tokio::test]
async fn model_switch_is_scoped_to_one_runtime_session() {
    let cwd = std::env::current_dir().unwrap();
    let initial = endpoint("large-a", "family-a", 8_000, Vec::new());
    let make = |id: &str| {
        let (updates, _) = mpsc::unbounded_channel();
        RuntimeSession::new_with_endpoint(
            id.into(),
            initial.clone(),
            Arc::new(FileLocks::new()),
            SessionTrust::for_headless_prompt(&cwd),
            cwd.clone(),
            updates,
            None,
        )
    };
    let first = make("first");
    let second = make("second");

    first
        .switch_model(PreparedModelSwitch {
            active: endpoint("small-b", "family-a", 4_000, Vec::new()),
        })
        .await
        .unwrap();

    assert_eq!(first.active_model().await.selection.model, "small-b");
    assert_eq!(second.active_model().await.selection.model, "large-a");
}

#[tokio::test]
async fn cross_family_switch_compacts_existing_assistant_history_with_new_model() {
    let cwd = std::env::current_dir().unwrap();
    let (updates, _) = mpsc::unbounded_channel();
    let session = RuntimeSession::new_with_endpoint(
        "cross-family".into(),
        endpoint("old", "family-a", 8_000, Vec::new()),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(&cwd),
        cwd,
        updates,
        None,
    );
    session
        .replace_history(vec![
            HistoryItem::System("system".into()),
            HistoryItem::User("continue implementing the requested feature".into()),
            HistoryItem::AssistantText("prior work ".repeat(2_000)),
        ])
        .await;

    let outcome = session
        .switch_model(PreparedModelSwitch {
            active: endpoint(
                "new",
                "family-b",
                32_000,
                vec![vec![StreamPiece::Text(healthy_summary())]],
            ),
        })
        .await
        .unwrap();

    assert_eq!(outcome.model, "new");
    assert!(
        outcome.compaction_warning.is_none(),
        "unexpected warning: {:?}",
        outcome.compaction_warning
    );
    assert!(
        session
            .history_snapshot()
            .await
            .iter()
            .any(|item| matches!(item, HistoryItem::CompactionSummary(_)))
    );
}

#[tokio::test]
async fn cross_family_switch_skips_compaction_without_model_authored_history() {
    let cwd = std::env::current_dir().unwrap();
    let (updates, _) = mpsc::unbounded_channel();
    let session = RuntimeSession::new_with_endpoint(
        "user-only".into(),
        endpoint("old", "family-a", 8_000, Vec::new()),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(&cwd),
        cwd,
        updates,
        None,
    );
    let original = vec![
        HistoryItem::System("system".into()),
        HistoryItem::User("an objective".into()),
    ];
    session.replace_history(original.clone()).await;

    session
        .switch_model(PreparedModelSwitch {
            active: endpoint("new", "family-b", 8_000, Vec::new()),
        })
        .await
        .unwrap();

    assert_eq!(session.history_snapshot().await, original);
}

#[tokio::test]
async fn ordinary_cross_family_compaction_failure_keeps_the_new_model_with_a_warning() {
    let cwd = std::env::current_dir().unwrap();
    let (updates, _) = mpsc::unbounded_channel();
    let session = RuntimeSession::new_with_endpoint(
        "warning".into(),
        endpoint("old", "family-a", 32_000, Vec::new()),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(&cwd),
        cwd,
        updates,
        None,
    );
    session
        .replace_history(vec![
            HistoryItem::System("system".into()),
            HistoryItem::User("retain the objective".into()),
            HistoryItem::AssistantText("prior work ".repeat(2_000)),
        ])
        .await;

    let outcome = session
        .switch_model(PreparedModelSwitch {
            active: endpoint(
                "new",
                "family-b",
                32_000,
                vec![vec![StreamPiece::Text("invalid summary".into())]],
            ),
        })
        .await
        .unwrap();

    assert!(outcome.compaction_warning.is_some());
    assert_eq!(session.active_model().await.selection.model, "new");
}

#[tokio::test]
async fn same_family_shrink_defers_compaction_until_the_next_sample() {
    let cwd = std::env::current_dir().unwrap();
    let (updates, _) = mpsc::unbounded_channel();
    let session = RuntimeSession::new_with_endpoint(
        "shrink".into(),
        endpoint("large", "family-a", 220_000, Vec::new()),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(&cwd),
        cwd,
        updates,
        None,
    );
    session
        .replace_history(vec![
            HistoryItem::System("system".into()),
            HistoryItem::User("retain the implementation state".into()),
            HistoryItem::AssistantText("large history payload ".repeat(22_500)),
        ])
        .await;

    session
        .switch_model(PreparedModelSwitch {
            active: endpoint(
                "small",
                "family-a",
                140_000,
                vec![
                    vec![StreamPiece::Text(healthy_summary())],
                    vec![StreamPiece::Text("continued".into())],
                ],
            ),
        })
        .await
        .unwrap();
    assert!(
        !session
            .history_snapshot()
            .await
            .iter()
            .any(|item| matches!(item, HistoryItem::CompactionSummary(_)))
    );

    let outcome = session.prompt("continue now".into()).await.unwrap();
    assert_eq!(
        outcome,
        RuntimePromptOutcome::Complete {
            text: "continued".into()
        }
    );
    assert!(
        session
            .history_snapshot()
            .await
            .iter()
            .any(|item| matches!(item, HistoryItem::CompactionSummary(_)))
    );
}

#[tokio::test]
async fn model_switch_rejects_a_busy_session() {
    let stream = Arc::new(BlockingStream {
        started: Notify::new(),
    });
    let (session, _) = session_with_stream(stream.clone());
    let prompt_session = session.clone();
    let prompt = tokio::spawn(async move { prompt_session.prompt("wait".into()).await });
    timeout(Duration::from_secs(1), stream.started.notified())
        .await
        .unwrap();

    let error = session
        .switch_model(PreparedModelSwitch {
            active: endpoint("new", "family-b", 8_000, Vec::new()),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, "runtime.session_busy");

    session.cancel().await.unwrap();
    prompt.await.unwrap().unwrap();
}
