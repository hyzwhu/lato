// Derived from: Codex@633ab199cfd724aa78013c006b27a2b3d049fc3b:codex-rs/rollout/src/recorder.rs
// License: Apache-2.0
// Lato changes: reduced the rollout writer to a bounded per-session journal command queue

use crate::file::{
    FileFaultInjector, append_envelope_blocking, replay_path_blocking, satisfy_existing_durability,
};
use lato_core::{JournalDurability, JournalEnvelope, JournalError, SessionId};
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
                            append_with_recovery(
                                &session_id,
                                &path,
                                *envelope,
                                durability,
                                faults.as_ref(),
                            )
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
