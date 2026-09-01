use async_trait::async_trait;
use lato_core::{
    CancelReason, Command, EventPayload, StartBehavior, StartTurn, TurnOutput, UserInput,
};
use lato_runtime::{TurnControl, TurnDriver, TurnEventEmitter, TurnRequest, spawn_session};
use std::{
    future::pending,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::oneshot;
use tokio::time::{Duration, timeout};

struct EchoDriver;

#[async_trait]
impl TurnDriver for EchoDriver {
    async fn run(
        &self,
        request: TurnRequest,
        _control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        events.model_delta(request.input.text.clone())?;
        Ok(TurnOutput {
            final_text: request.input.text,
        })
    }
}

struct BlockingDriver;

#[async_trait]
impl TurnDriver for BlockingDriver {
    async fn run(
        &self,
        _request: TurnRequest,
        mut control: TurnControl,
        _events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        tokio::select! {
            _ = control.cancellation.cancelled() => Ok(TurnOutput { final_text: String::new() }),
            steer = control.steering.recv() => Ok(TurnOutput {
                final_text: format!("steered:{}", steer.expect("steering channel closed").text),
            }),
        }
    }
}

struct StaleEventDriver {
    runs: AtomicUsize,
    first_emitter_tx: Mutex<Option<oneshot::Sender<TurnEventEmitter>>>,
}

#[async_trait]
impl TurnDriver for StaleEventDriver {
    async fn run(
        &self,
        _request: TurnRequest,
        mut control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        if self.runs.fetch_add(1, Ordering::SeqCst) == 0 {
            let sent = self
                .first_emitter_tx
                .lock()
                .expect("first emitter mutex poisoned")
                .take()
                .expect("first emitter sender already taken")
                .send(events);
            assert!(sent.is_ok(), "first emitter receiver closed");
        }
        tokio::select! {
            _ = control.cancellation.cancelled() => Ok(TurnOutput { final_text: String::new() }),
            steer = control.steering.recv() => Ok(TurnOutput {
                final_text: format!("steered:{}", steer.expect("steering channel closed").text),
            }),
        }
    }
}

struct BurstDriver;

#[async_trait]
impl TurnDriver for BurstDriver {
    async fn run(
        &self,
        _request: TurnRequest,
        _control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        for index in 0..300 {
            events.model_delta(format!("delta-{index}"))?;
        }
        Ok(TurnOutput {
            final_text: "done".into(),
        })
    }
}

struct CleanupDriver {
    emitter_tx: Mutex<Option<oneshot::Sender<TurnEventEmitter>>>,
    dropped_tx: Mutex<Option<oneshot::Sender<()>>>,
}

struct DropSignal(Option<oneshot::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

#[async_trait]
impl TurnDriver for CleanupDriver {
    async fn run(
        &self,
        _request: TurnRequest,
        _control: TurnControl,
        events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        let _drop_signal = DropSignal(
            self.dropped_tx
                .lock()
                .expect("dropped signal mutex poisoned")
                .take(),
        );
        let sent = self
            .emitter_tx
            .lock()
            .expect("emitter mutex poisoned")
            .take()
            .expect("emitter sender already taken")
            .send(events);
        assert!(sent.is_ok(), "emitter receiver closed");
        pending().await
    }
}

async fn next_event(
    events: &mut tokio::sync::broadcast::Receiver<lato_core::EventEnvelope>,
) -> lato_core::EventEnvelope {
    timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

#[tokio::test]
async fn emits_ordered_start_delta_and_completion() {
    let session = spawn_session("session-1".into(), Arc::new(EchoDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("hello"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    let started = next_event(&mut events).await;
    let turn_started = next_event(&mut events).await;
    let delta = next_event(&mut events).await;
    let completed = next_event(&mut events).await;

    assert!(matches!(started.payload, EventPayload::SessionStarted));
    assert!(matches!(turn_started.payload, EventPayload::TurnStarted));
    assert_eq!(
        delta.payload,
        EventPayload::ModelDelta {
            text: "hello".into()
        }
    );
    assert_eq!(
        completed.payload,
        EventPayload::TurnCompleted(TurnOutput {
            final_text: "hello".into()
        }),
    );
    assert_eq!(
        [
            started.sequence,
            turn_started.sequence,
            delta.sequence,
            completed.sequence
        ],
        [1, 2, 3, 4],
    );
}

#[tokio::test]
async fn steer_is_delivered_to_the_active_turn() {
    let session = spawn_session("session-2".into(), Arc::new(BlockingDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("start"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    session
        .submit(Command::SteerTurn(UserInput::text("more")))
        .await
        .unwrap();

    let mut final_text = None;
    for _ in 0..4 {
        if let EventPayload::TurnCompleted(output) = next_event(&mut events).await.payload {
            final_text = Some(output.final_text);
            break;
        }
    }
    assert_eq!(final_text.as_deref(), Some("steered:more"));
}

#[tokio::test]
async fn replace_cancels_before_starting_the_pending_turn() {
    let session = spawn_session("session-3".into(), Arc::new(BlockingDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("first"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("second"),
            behavior: StartBehavior::Replace,
        }))
        .await
        .unwrap();

    let session_started = next_event(&mut events).await;
    let first_started = next_event(&mut events).await;
    assert!(matches!(
        session_started.payload,
        EventPayload::SessionStarted
    ));
    assert!(matches!(first_started.payload, EventPayload::TurnStarted));
    let first_turn_id = first_started.turn_id.expect("first turn id missing");

    let mut cancelled_sequence = None;
    let mut second_start = None;
    for _ in 0..6 {
        let event = next_event(&mut events).await;
        match event.payload {
            EventPayload::TurnCancelled {
                reason: CancelReason::Replaced,
            } => {
                assert_eq!(event.turn_id.as_ref(), Some(&first_turn_id));
                cancelled_sequence = Some(event.sequence);
            }
            EventPayload::TurnStarted if cancelled_sequence.is_some() => {
                second_start = Some(event);
                break;
            }
            _ => {}
        }
    }
    let cancelled_sequence = cancelled_sequence.expect("replacement cancellation missing");
    let second_start = second_start.expect("pending turn did not start after cancellation");
    assert_eq!(second_start.sequence, cancelled_sequence + 1);
    assert_ne!(second_start.turn_id.as_ref(), Some(&first_turn_id));
}

#[tokio::test]
async fn event_identity_is_stable_and_unique_within_a_session() {
    let session = spawn_session("session-identity".into(), Arc::new(EchoDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("hello"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    let mut seen_ids = std::collections::HashSet::new();
    let mut turn_id = None;
    for expected_sequence in 1..=4 {
        let event = next_event(&mut events).await;
        assert_eq!(event.session_id.as_str(), "session-identity");
        assert_eq!(event.sequence, expected_sequence);
        assert!(seen_ids.insert(event.event_id.clone()));
        if matches!(event.payload, EventPayload::TurnStarted) {
            turn_id = event.turn_id.clone();
        } else if matches!(
            event.payload,
            EventPayload::ModelDelta { .. } | EventPayload::TurnCompleted(_)
        ) {
            assert_eq!(event.turn_id, turn_id);
        }
    }
}

#[tokio::test]
async fn dropping_the_last_handle_aborts_the_driver_and_closes_its_event_bus() {
    let (emitter_tx, emitter_rx) = oneshot::channel();
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let driver = CleanupDriver {
        emitter_tx: Mutex::new(Some(emitter_tx)),
        dropped_tx: Mutex::new(Some(dropped_tx)),
    };
    let session = spawn_session("session-cleanup".into(), Arc::new(driver));
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("wait"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let emitter = timeout(Duration::from_secs(1), emitter_rx)
        .await
        .expect("driver did not expose its emitter")
        .expect("driver dropped its emitter sender");

    drop(session);

    timeout(Duration::from_secs(1), dropped_rx)
        .await
        .expect("driver task was not cleaned up")
        .expect("driver dropped signal sender unexpectedly");
    let error = emitter
        .model_delta("after shutdown")
        .expect_err("closed runtime event bus should reject driver events");
    assert_eq!(error.code, "runtime.event_bus_closed");
}

#[tokio::test]
async fn reject_mode_does_not_start_a_second_turn() {
    let session = spawn_session("session-reject".into(), Arc::new(BlockingDriver));
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("first"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let error = session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("second"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code, "runtime.invalid_transition");
}

#[tokio::test]
async fn cancel_targets_the_active_turn_and_emits_user_reason() {
    let session = spawn_session("session-cancel".into(), Arc::new(BlockingDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("wait"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let started = next_event(&mut events).await;
    let turn_started = next_event(&mut events).await;
    let turn_id = turn_started.turn_id.unwrap();
    assert!(matches!(started.payload, EventPayload::SessionStarted));
    session
        .submit(Command::CancelTurn { turn_id })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await.payload,
        EventPayload::TurnCancelled {
            reason: CancelReason::User,
        },
    );
}

#[tokio::test]
async fn shutdown_stops_the_session_and_rejects_future_commands() {
    let session = spawn_session("session-stop".into(), Arc::new(BlockingDriver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("wait"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::SessionStarted
    ));
    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::TurnStarted
    ));
    session.submit(Command::Shutdown).await.unwrap();
    assert_eq!(
        next_event(&mut events).await.payload,
        EventPayload::TurnCancelled {
            reason: CancelReason::Shutdown,
        },
    );
    assert!(matches!(
        next_event(&mut events).await.payload,
        EventPayload::SessionStopped
    ));
    let error = session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("after stop"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code, "runtime.command_bus_closed");
}

#[tokio::test]
async fn stale_driver_events_from_a_replaced_turn_are_ignored() {
    let (first_emitter_tx, first_emitter_rx) = oneshot::channel();
    let driver = StaleEventDriver {
        runs: AtomicUsize::new(0),
        first_emitter_tx: Mutex::new(Some(first_emitter_tx)),
    };
    let session = spawn_session("session-stale".into(), Arc::new(driver));
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("first"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    let stale_emitter = timeout(Duration::from_secs(1), first_emitter_rx)
        .await
        .expect("first driver did not expose its emitter")
        .expect("first driver dropped its emitter sender");
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("second"),
            behavior: StartBehavior::Replace,
        }))
        .await
        .unwrap();

    loop {
        let event = next_event(&mut events).await;
        if matches!(event.payload, EventPayload::TurnStarted) && event.sequence > 2 {
            break;
        }
    }
    stale_emitter
        .model_delta("stale")
        .expect("session should still accept driver messages");
    session
        .submit(Command::SteerTurn(UserInput::text("finish")))
        .await
        .unwrap();

    loop {
        match next_event(&mut events).await.payload {
            EventPayload::ModelDelta { text } => panic!("stale event leaked: {text}"),
            EventPayload::TurnCompleted(output) => {
                assert_eq!(output.final_text, "steered:finish");
                break;
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn slow_subscribers_observe_bounded_event_lag() {
    let session = spawn_session("session-lag".into(), Arc::new(BurstDriver));
    let mut lagged_events = session.subscribe();
    let mut completion_events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("burst"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();

    loop {
        let event = next_event(&mut completion_events).await;
        if matches!(event.payload, EventPayload::TurnCompleted(_)) {
            break;
        }
    }

    let lagged = timeout(Duration::from_secs(1), lagged_events.recv())
        .await
        .expect("lag check timed out")
        .expect_err("slow subscriber should receive a bounded lag signal");
    assert!(matches!(
        lagged,
        tokio::sync::broadcast::error::RecvError::Lagged(skipped) if skipped > 0
    ));
}
