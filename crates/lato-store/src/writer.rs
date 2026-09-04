// Derived from: Codex@633ab199cfd724aa78013c006b27a2b3d049fc3b:codex-rs/rollout/src/recorder.rs
// License: Apache-2.0
// Lato changes: reduced the rollout writer to a bounded per-session journal command queue
// Phase 4B derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/persistence.rs

use crate::file::{
    FaultPoint, FileFaultInjector, append_envelope_blocking, replay_path_blocking,
    replay_with_projection_blocking, satisfy_existing_durability,
};
use crate::projection;
use lato_core::{
    HISTORY_PROJECTION_SCHEMA_VERSION, HistoryCheckpoint, HistoryProjectionMetadata,
    HistoryReplacementReason, JournalDurability, JournalEnvelope, JournalError, JournalRecord,
    JournalRecordId, ModelMessage, ProjectionError, SessionId, checkpoint_digest, history_digest,
};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, oneshot};

const WRITER_CAPACITY: usize = 128;

#[derive(Clone)]
pub(crate) struct WriterHandle {
    tx: mpsc::Sender<WriterCommand>,
}

enum WriterCommand {
    Append {
        envelope: Box<JournalEnvelope>,
        durability: JournalDurability,
        ack: oneshot::Sender<Result<(), JournalError>>,
    },
    Shutdown {
        ack: oneshot::Sender<Result<(), JournalError>>,
    },
    ReplaceHistory {
        messages: Vec<ModelMessage>,
        reason: HistoryReplacementReason,
        ack: oneshot::Sender<Result<HistoryProjectionMetadata, ProjectionError>>,
    },
    Replay {
        ack: oneshot::Sender<Result<lato_core::JournalReplay, JournalError>>,
    },
}

impl WriterHandle {
    pub(crate) fn spawn(
        session_id: SessionId,
        path: PathBuf,
        faults: Arc<dyn FileFaultInjector>,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel(WRITER_CAPACITY);
        tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                match command {
                    WriterCommand::Append {
                        envelope,
                        durability,
                        ack,
                    } => {
                        let session_id = session_id.clone();
                        let path = path.clone();
                        let faults = faults.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            let projection_envelope = envelope.clone();
                            let result = append_with_recovery(
                                &session_id,
                                &path,
                                *envelope,
                                durability,
                                faults.as_ref(),
                            );
                            if result.is_ok()
                                && let Err(error) = projection::apply_committed_envelope(
                                    &session_id,
                                    &path,
                                    &projection_envelope,
                                )
                            {
                                eprintln!("warning [{}]: {error}", error.code());
                            }
                            result
                        })
                        .await
                        .map_err(|error| JournalError::Io {
                            message: error.to_string(),
                        })
                        .and_then(|result| result);
                        let _ = ack.send(result);
                    }
                    WriterCommand::Shutdown { ack } => {
                        let _ = ack.send(Ok(()));
                        break;
                    }
                    WriterCommand::ReplaceHistory {
                        messages,
                        reason,
                        ack,
                    } => {
                        let result = replace_history_blocking(
                            &session_id,
                            &path,
                            messages,
                            reason,
                            faults.as_ref(),
                        );
                        let _ = ack.send(result);
                    }
                    WriterCommand::Replay { ack } => {
                        let _ = ack.send(replay_with_projection_blocking(&session_id, &path));
                    }
                }
            }
        });
        Self { tx }
    }

    pub(crate) async fn append(
        &self,
        envelope: JournalEnvelope,
        durability: JournalDurability,
    ) -> Result<(), JournalError> {
        let (ack, result) = oneshot::channel();
        self.tx
            .send(WriterCommand::Append {
                envelope: Box::new(envelope),
                durability,
                ack,
            })
            .await
            .map_err(|error| JournalError::Io {
                message: format!("journal writer closed: {error}"),
            })?;
        result.await.map_err(|error| JournalError::Io {
            message: format!("journal writer dropped acknowledgement: {error}"),
        })?
    }

    pub(crate) async fn shutdown(&self) -> Result<(), JournalError> {
        let (ack, result) = oneshot::channel();
        self.tx
            .send(WriterCommand::Shutdown { ack })
            .await
            .map_err(|error| JournalError::Io {
                message: format!("journal writer closed: {error}"),
            })?;
        result.await.map_err(|error| JournalError::Io {
            message: format!("journal writer dropped acknowledgement: {error}"),
        })?
    }

    pub(crate) async fn replace_history(
        &self,
        messages: Vec<ModelMessage>,
        reason: HistoryReplacementReason,
    ) -> Result<HistoryProjectionMetadata, ProjectionError> {
        let (ack, result) = oneshot::channel();
        self.tx
            .send(WriterCommand::ReplaceHistory {
                messages,
                reason,
                ack,
            })
            .await
            .map_err(|error| ProjectionError::WriteFailed {
                message: format!("journal writer closed: {error}"),
            })?;
        result.await.map_err(|error| ProjectionError::WriteFailed {
            message: format!("journal writer dropped acknowledgement: {error}"),
        })?
    }

    pub(crate) async fn replay(&self) -> Result<lato_core::JournalReplay, JournalError> {
        let (ack, result) = oneshot::channel();
        self.tx
            .send(WriterCommand::Replay { ack })
            .await
            .map_err(|error| JournalError::Io {
                message: format!("journal writer closed: {error}"),
            })?;
        result.await.map_err(|error| JournalError::Io {
            message: format!("journal writer dropped acknowledgement: {error}"),
        })?
    }
}

fn replace_history_blocking(
    session_id: &SessionId,
    journal_path: &std::path::Path,
    messages: Vec<ModelMessage>,
    reason: HistoryReplacementReason,
    faults: &dyn FileFaultInjector,
) -> Result<HistoryProjectionMetadata, ProjectionError> {
    let replay =
        replay_path_blocking(session_id, journal_path).map_err(projection_journal_error)?;
    if replay.envelopes.is_empty() {
        return Err(ProjectionError::Divergent {
            message: "cannot replace history for an empty journal".into(),
        });
    }
    if !replay.projection.unresolved_tools.is_empty() {
        return Err(ProjectionError::Divergent {
            message: "cannot replace history with an unresolved prepared tool".into(),
        });
    }
    let previous = replay.envelopes.last().expect("checked non-empty");
    let sequence = replay.projection.next_journal_sequence;
    let record_id = JournalRecordId::from(format!("{session_id}-history-replacement-{sequence}"));
    let prior_checkpoint_id = replay.projection.active_checkpoint_id.clone();
    let digest = checkpoint_digest(
        session_id,
        previous.journal_sequence,
        &previous.record_id,
        prior_checkpoint_id.as_deref(),
        &messages,
    )?;
    let checkpoint_id = format!(
        "cp-{}-{}",
        sequence,
        digest
            .trim_start_matches("sha256:v1:")
            .get(..16)
            .unwrap_or("digest")
    );
    let checkpoint = HistoryCheckpoint {
        schema_version: HISTORY_PROJECTION_SCHEMA_VERSION,
        checkpoint_id: checkpoint_id.clone(),
        session_id: session_id.clone(),
        replaced_through_sequence: previous.journal_sequence,
        replaced_through_record_id: previous.record_id.clone(),
        prior_checkpoint_id: prior_checkpoint_id.clone(),
        messages: messages.clone(),
        content_digest: digest.clone(),
    };
    let session_dir = journal_path
        .parent()
        .ok_or_else(|| ProjectionError::WriteFailed {
            message: "journal has no session directory".into(),
        })?;
    let paths = projection::ProjectionPaths::new(session_dir);
    faults
        .check(FaultPoint::BeforeCheckpointPublish)
        .map_err(projection_journal_error)?;
    projection::publish_checkpoint(&paths, &checkpoint)?;
    let marker = JournalEnvelope {
        schema_version: lato_core::JOURNAL_SCHEMA_VERSION,
        record_id,
        session_id: session_id.clone(),
        turn_id: None,
        journal_sequence: sequence,
        timestamp_ms: now_ms(),
        record: JournalRecord::HistoryProjectionReplaced {
            checkpoint_id: checkpoint_id.clone(),
            checkpoint_digest: digest,
            replaced_through_sequence: previous.journal_sequence,
            replaced_through_record_id: previous.record_id.clone(),
            replacement_entry_count: messages.len() as u64,
            history_digest: history_digest(&messages)?,
            reason,
            prior_checkpoint_id,
        },
    };
    append_with_recovery(
        session_id,
        journal_path,
        marker,
        JournalDurability::SyncData,
        faults,
    )
    .map_err(projection_journal_error)?;
    let replay =
        replay_path_blocking(session_id, journal_path).map_err(projection_journal_error)?;
    let generation = replay
        .envelopes
        .iter()
        .filter(|envelope| {
            matches!(
                envelope.record,
                JournalRecord::HistoryProjectionReplaced { .. }
            )
        })
        .count() as u64;
    projection::publish_replacement(
        session_id,
        &paths,
        &replay.envelopes,
        &messages,
        generation,
        checkpoint_id,
        faults,
    )
}

fn projection_journal_error(error: JournalError) -> ProjectionError {
    match error {
        JournalError::Projection(error) => error,
        other => ProjectionError::WriteFailed {
            message: other.to_string(),
        },
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn append_with_recovery(
    session_id: &SessionId,
    path: &std::path::Path,
    envelope: JournalEnvelope,
    durability: JournalDurability,
    faults: &dyn FileFaultInjector,
) -> Result<(), JournalError> {
    match append_envelope_blocking(session_id, path, &envelope, durability, faults) {
        Ok(()) => Ok(()),
        Err(first @ JournalError::Io { .. }) => {
            let replay = replay_path_blocking(session_id, path)?;
            if replay
                .envelopes
                .last()
                .is_some_and(|saved| saved.record_id == envelope.record_id)
            {
                return satisfy_existing_durability(path, durability);
            }
            append_envelope_blocking(session_id, path, &envelope, durability, faults).map_err(
                |second| JournalError::Io {
                    message: format!("{first}; retry failed: {second}"),
                },
            )
        }
        Err(error) => Err(error),
    }
}
