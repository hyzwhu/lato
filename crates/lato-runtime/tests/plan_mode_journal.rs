//! T6 — Plan-mode journal trace: transitions and the approval record are
//! durable non-conversation journal records (spec §5), reproducible by
//! `/plan status` at the session layer.

use lato_core::JournalReplay;
use lato_core::{
    Command, EventStore, JournalRecord, PlanCommand, PlanModeJournalEvent, PlanPhase, SessionId,
    StartBehavior, StartTurn, TurnOutput, UserInput,
};
use lato_runtime::{
    SessionBootstrap, TurnControl, TurnDriver, TurnEventEmitter, TurnRequest,
    spawn_session_with_store,
};
use lato_store::MemoryEventStore;
use std::sync::Arc;

struct NoopDriver;

#[async_trait::async_trait]
impl TurnDriver for NoopDriver {
    async fn run(
        &self,
        request: TurnRequest,
        _control: TurnControl,
        _events: TurnEventEmitter,
    ) -> Result<TurnOutput, lato_core::AgentError> {
        Ok(TurnOutput {
            final_text: request.input.text,
        })
    }
}

fn bootstrap(session_id: &SessionId) -> SessionBootstrap {
    SessionBootstrap {
        replay: JournalReplay::empty(session_id.clone()),
    }
}

#[tokio::test]
async fn plan_mode_events_are_appended_as_durable_journal_records() {
    let sid = SessionId::from("plan-mode-journal");
    let store = Arc::new(MemoryEventStore::new());
    let session = spawn_session_with_store(
        sid.clone(),
        Arc::new(NoopDriver),
        store.clone(),
        bootstrap(&sid),
    );
    session
        .submit(Command::RecordPlanModeEvent {
            event: PlanModeJournalEvent::Transitioned {
                activation: 1,
                from: PlanPhase::Inactive,
                to: PlanPhase::Drafting,
                command: PlanCommand::Enter,
            },
        })
        .await
        .unwrap();
    session
        .submit(Command::RecordPlanModeEvent {
            event: PlanModeJournalEvent::ApprovalRecorded {
                activation: 1,
                generation: 1,
                content_hash: "hash-1".into(),
                approver: "user".into(),
                approved_at_ms: 1_789_531_030_426,
            },
        })
        .await
        .unwrap();
    session
        .submit(Command::RecordPlanModeEvent {
            event: PlanModeJournalEvent::ApprovalRevoked {
                activation: 1,
                generation: 1,
                reason: "plan.approval_stale".into(),
            },
        })
        .await
        .unwrap();

    let replay = store.replay(&sid).await.unwrap();
    let records: Vec<JournalRecord> = replay
        .envelopes
        .iter()
        .map(|envelope| envelope.record.clone())
        .collect();
    assert!(records.contains(&JournalRecord::PlanModeTransitioned {
        activation: 1,
        from: PlanPhase::Inactive,
        to: PlanPhase::Drafting,
        command: PlanCommand::Enter,
    }));
    assert!(records.contains(&JournalRecord::PlanApprovalRecorded {
        activation: 1,
        generation: 1,
        content_hash: "hash-1".into(),
        approver: "user".into(),
        approved_at_ms: 1_789_531_030_426,
    }));
    assert!(records.contains(&JournalRecord::PlanApprovalRevoked {
        activation: 1,
        generation: 1,
        reason: "plan.approval_stale".into(),
    }));
}

#[tokio::test]
async fn plan_journal_records_stay_out_of_conversation_projection() {
    let sid = SessionId::from("plan-mode-journal-projection");
    let store = Arc::new(MemoryEventStore::new());
    let session = spawn_session_with_store(
        sid.clone(),
        Arc::new(NoopDriver),
        store.clone(),
        bootstrap(&sid),
    );
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("hello"),
            behavior: StartBehavior::Reject,
        }))
        .await
        .unwrap();
    session
        .submit(Command::RecordPlanModeEvent {
            event: PlanModeJournalEvent::Transitioned {
                activation: 1,
                from: PlanPhase::Inactive,
                to: PlanPhase::Drafting,
                command: PlanCommand::Enter,
            },
        })
        .await
        .unwrap();

    let replay = store.replay(&sid).await.unwrap();
    let projection = lato_core::project_journal(&sid, &replay.envelopes).unwrap();
    // The plan transition is a non-conversation event: the projected
    // conversation holds only the user turn, never the plan record.
    assert_eq!(projection.messages.len(), 1);
}
