// Phase 7C3: the SessionLoop is the ONLY writer of AgentField journal
// events — sequence allocation stays with the single sequence owner, the
// envelope carries the 7C3 schema version, and the sequence stays
// continuous with records appended before and after.

use lato_core::{
    AgentFieldJournalEvent, AgentFieldRunStatus, Command, EventStore, JournalRecord,
    ModelSelection, SessionId, StartTurn, TurnOutput, UserInput, validate_agentfield_event,
};
use lato_runtime::{
    SessionBootstrap, TurnControl, TurnDriver, TurnEventEmitter, TurnRequest,
    spawn_session_with_store,
};
use lato_store::MemoryEventStore;
use std::sync::Arc;

struct EchoDriver;

#[async_trait::async_trait]
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

fn intent_event(session_id: &str) -> AgentFieldJournalEvent {
    AgentFieldJournalEvent::AgentFieldRunIntentRecorded {
        run_id: "afrun_probe-1".into(),
        session_id: session_id.to_owned(),
        alias: "contract-review".into(),
        execute_target: "legal.review_contract".into(),
        catalog_revision: "sha256:rev-1".into(),
        input_digest: "a".repeat(64),
        created_at_ms: 1_000,
    }
}

#[tokio::test]
async fn agentfield_events_are_appended_by_the_session_loop_with_schema_v2() {
    let sid = SessionId::from("agentfield-loop");
    let store = Arc::new(MemoryEventStore::new());
    let session = spawn_session_with_store(
        sid.clone(),
        Arc::new(EchoDriver),
        store.clone(),
        SessionBootstrap {
            replay: lato_core::JournalReplay::empty(sid.clone()),
        },
    );
    session.await_started().await.unwrap();

    // Append through the single writer; the reply resolves only after the
    // durable commit.
    session
        .submit(Command::RecordAgentFieldEvent {
            event: intent_event(sid.as_str()),
        })
        .await
        .unwrap();
    // Interleave a non-agentfield record to prove sequence continuity.
    session
        .submit(Command::SelectModel {
            selection: ModelSelection::new("fixture", "large-a").unwrap(),
            model_family: None,
            context_window: None,
        })
        .await
        .unwrap();
    session
        .submit(Command::RecordAgentFieldEvent {
            event: AgentFieldJournalEvent::AgentFieldExecutionBound {
                run_id: "afrun_probe-1".into(),
                session_id: sid.as_str().to_owned(),
                execution_id: "exec-1".into(),
                alias: "contract-review".into(),
                catalog_revision: "sha256:rev-1".into(),
                input_digest: "a".repeat(64),
                bound_at_ms: 1_100,
            },
        })
        .await
        .unwrap();

    let replay = store.replay(&sid).await.unwrap();
    assert!(replay.exists);
    // Sequence 0 is SessionStarted; our events continue 1..3.
    assert_eq!(replay.envelopes.len(), 4);
    assert!(matches!(
        replay.envelopes[1].record,
        JournalRecord::AgentField { .. }
    ));
    assert_eq!(
        replay.envelopes[1].schema_version,
        lato_core::AGENTFIELD_JOURNAL_SCHEMA_VERSION
    );
    assert!(matches!(
        replay.envelopes[2].record,
        JournalRecord::ModelSelected { .. }
    ));
    assert_eq!(
        replay.envelopes[2].schema_version,
        lato_core::JOURNAL_SCHEMA_VERSION
    );
    assert!(matches!(
        replay.envelopes[3].record,
        JournalRecord::AgentField { .. }
    ));
    // The projection rebuilds the bound run (zero network).
    assert_eq!(replay.projection.agentfield_runs.len(), 1);
    let run = &replay.projection.agentfield_runs[0];
    assert_eq!(run.run_id, "afrun_probe-1");
    assert_eq!(run.execution_id.as_deref(), Some("exec-1"));
    assert_eq!(run.status, AgentFieldRunStatus::Queued);
}

#[tokio::test]
async fn oversized_agentfield_events_are_rejected_before_any_append() {
    let sid = SessionId::from("agentfield-loop-oversize");
    let store = Arc::new(MemoryEventStore::new());
    let session = spawn_session_with_store(
        sid.clone(),
        Arc::new(EchoDriver),
        store.clone(),
        SessionBootstrap {
            replay: lato_core::JournalReplay::empty(sid.clone()),
        },
    );
    session.await_started().await.unwrap();

    let mut oversized = intent_event(sid.as_str());
    if let AgentFieldJournalEvent::AgentFieldRunIntentRecorded { alias, .. } = &mut oversized {
        *alias = "x".repeat(lato_core::AGENTFIELD_MAX_ALIAS_BYTES + 1);
    }
    assert!(validate_agentfield_event(&oversized).is_err());
    let error = session
        .submit(Command::RecordAgentFieldEvent { event: oversized })
        .await
        .unwrap_err();
    assert_eq!(error.code, "journal.corrupt");
    // Nothing was appended after the SessionStarted record.
    let replay = store.replay(&sid).await.unwrap();
    assert_eq!(replay.envelopes.len(), 1);
}

#[tokio::test]
async fn a_turn_can_still_run_alongside_agentfield_appends() {
    let sid = SessionId::from("agentfield-loop-turn");
    let store = Arc::new(MemoryEventStore::new());
    let session = spawn_session_with_store(
        sid.clone(),
        Arc::new(EchoDriver),
        store.clone(),
        SessionBootstrap {
            replay: lato_core::JournalReplay::empty(sid.clone()),
        },
    );
    session.await_started().await.unwrap();
    session
        .submit(Command::RecordAgentFieldEvent {
            event: intent_event(sid.as_str()),
        })
        .await
        .unwrap();
    let mut events = session.subscribe();
    session
        .submit(Command::StartTurn(StartTurn {
            input: UserInput::text("hello"),
            behavior: lato_core::StartBehavior::Reject,
        }))
        .await
        .unwrap();
    // Wait for the turn to complete.
    while !events.recv().await.is_ok_and(|envelope| {
        matches!(
            envelope.payload,
            lato_core::EventPayload::TurnCompleted { .. }
        )
    }) {}
    let replay = store.replay(&sid).await.unwrap();
    let sequences: Vec<u64> = replay
        .envelopes
        .iter()
        .map(|envelope| envelope.journal_sequence)
        .collect();
    assert_eq!(sequences, (0..sequences.len() as u64).collect::<Vec<_>>());
}
