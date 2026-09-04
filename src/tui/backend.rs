use crate::client::{ClientUpdate, InteractiveAcpClient, SessionSummary};
use lato_agent::{ApprovalRequest, ToolApproval};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub enum BackendCommand {
    Submit(String),
    Cancel,
    Clear,
    Resume(String),
    RenameSession { session_id: String, title: String },
    DeleteSession(String),
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendEvent {
    SessionReady(String),
    Update(ClientUpdate),
    TurnCompleted(String),
    TurnCancelled,
    Cleared(String),
    Resumed(String),
    Sessions(Vec<SessionSummary>),
    SessionRenamed(SessionSummary),
    SessionDeleted {
        session_id: String,
        replacement_session_id: Option<String>,
        sessions: Vec<SessionSummary>,
    },
    Error(String),
}

#[derive(Debug)]
pub struct ApprovalPrompt {
    pub tool: String,
    pub summary: String,
    pub response: oneshot::Sender<bool>,
}

pub struct TuiToolApproval {
    sender: mpsc::UnboundedSender<ApprovalPrompt>,
}

impl TuiToolApproval {
    pub fn channel() -> (
        Arc<dyn ToolApproval>,
        mpsc::UnboundedReceiver<ApprovalPrompt>,
    ) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Arc::new(Self { sender }), receiver)
    }
}

#[async_trait::async_trait]
impl ToolApproval for TuiToolApproval {
    async fn approve(&self, request: &ApprovalRequest) -> bool {
        let (response, answer) = oneshot::channel();
        if self
            .sender
            .send(ApprovalPrompt {
                tool: request.request.tool_name.to_string(),
                summary: request.summary.clone(),
                response,
            })
            .is_err()
        {
            return false;
        }
        answer.await.unwrap_or(false)
    }
}

#[derive(Clone)]
pub struct BackendHandle {
    commands: mpsc::UnboundedSender<BackendCommand>,
}

impl BackendHandle {
    pub fn send(&self, command: BackendCommand) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "TUI backend stopped".to_string())
    }
}

type ActiveTurn = tokio::task::JoinHandle<(InteractiveAcpClient, TurnEnd)>;

#[derive(Debug)]
enum TurnEnd {
    Completed(String),
    Cancelled(Result<(), String>),
    Failed(String),
}

pub fn spawn(
    client: InteractiveAcpClient,
) -> (BackendHandle, mpsc::UnboundedReceiver<BackendEvent>) {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let handle = BackendHandle {
        commands: command_tx,
    };
    tokio::task::spawn_local(async move {
        let mut client = Some(client);
        let mut active: Option<ActiveTurn> = None;
        let mut cancel: Option<oneshot::Sender<()>> = None;
        if let Some(session) = client.as_ref().map(InteractiveAcpClient::session_id) {
            let _ = event_tx.send(BackendEvent::SessionReady(session.to_string()));
        }

        loop {
            if let Some(turn) = active.as_mut() {
                tokio::select! {
                    command = command_rx.recv() => match command {
                        Some(BackendCommand::Cancel) => {
                            if let Some(signal) = cancel.take() {
                                let _ = signal.send(());
                            }
                        }
                        Some(BackendCommand::Shutdown) | None => {
                            if let Some(signal) = cancel.take() {
                                let _ = signal.send(());
                            }
                            break;
                        }
                        Some(BackendCommand::Submit(_)) => {
                            let _ = event_tx.send(BackendEvent::Error("a turn is already running".into()));
                        }
                        Some(BackendCommand::Resume(_)) => {
                            let _ = event_tx.send(BackendEvent::Error("cannot switch sessions while a turn is running".into()));
                        }
                        Some(BackendCommand::RenameSession { .. } | BackendCommand::DeleteSession(_)) => {
                            let _ = event_tx.send(BackendEvent::Error("cannot change sessions while a turn is running".into()));
                        }
                        Some(BackendCommand::Clear) => {
                            let _ = event_tx.send(BackendEvent::Error("cannot clear while a turn is running".into()));
                        }
                    },
                    result = turn => {
                        match result {
                            Ok((mut returned, TurnEnd::Completed(text))) => {
                                let _ = event_tx.send(BackendEvent::TurnCompleted(text));
                                if let Ok(sessions) = returned.list_session_summaries().await {
                                    let _ = event_tx.send(BackendEvent::Sessions(sessions));
                                }
                                client = Some(returned);
                            }
                            Ok((returned, TurnEnd::Cancelled(cancelled))) => {
                                client = Some(returned);
                                match cancelled {
                                    Ok(()) => { let _ = event_tx.send(BackendEvent::TurnCancelled); }
                                    Err(error) => { let _ = event_tx.send(BackendEvent::Error(error)); }
                                }
                            }
                            Ok((returned, TurnEnd::Failed(error))) => {
                                client = Some(returned);
                                let _ = event_tx.send(BackendEvent::Error(error));
                            }
                            Err(error) => {
                                let _ = event_tx.send(BackendEvent::Error(format!("turn task failed: {error}")));
                            }
                        }
                        active = None;
                        cancel = None;
                    }
                }
                continue;
            }

            match command_rx.recv().await {
                Some(BackendCommand::Submit(text)) => {
                    let Some(mut owned) = client.take() else {
                        let _ = event_tx.send(BackendEvent::Error("session is unavailable".into()));
                        continue;
                    };
                    let updates = event_tx.clone();
                    let (cancel_tx, cancel_rx) = oneshot::channel();
                    cancel = Some(cancel_tx);
                    active = Some(tokio::task::spawn_local(async move {
                        let cancelled;
                        let response;
                        {
                            let send = owned.send_streaming(text, |raw| {
                                let update = ClientUpdate::from_json(raw);
                                if update != ClientUpdate::Unknown {
                                    let _ = updates.send(BackendEvent::Update(update));
                                }
                            });
                            tokio::pin!(send);
                            tokio::select! {
                                result = &mut send => {
                                    response = Some(result);
                                    cancelled = false;
                                }
                                _ = cancel_rx => {
                                    response = None;
                                    cancelled = true;
                                }
                            }
                        }
                        if cancelled {
                            let result = owned.cancel().await;
                            (owned, TurnEnd::Cancelled(result))
                        } else {
                            let result = response.expect("completed branch sets response");
                            match result {
                                Ok(text) => (owned, TurnEnd::Completed(text)),
                                Err(error) => (owned, TurnEnd::Failed(error)),
                            }
                        }
                    }));
                }
                Some(BackendCommand::Cancel) => {
                    let _ = event_tx.send(BackendEvent::TurnCancelled);
                }
                Some(BackendCommand::Clear) => {
                    let Some(owned) = client.as_mut() else {
                        let _ = event_tx.send(BackendEvent::Error("session is unavailable".into()));
                        continue;
                    };
                    match owned.clear().await {
                        Ok(()) => {
                            let _ = event_tx
                                .send(BackendEvent::Cleared(owned.session_id().to_string()));
                            if let Ok(sessions) = owned.list_session_summaries().await {
                                let _ = event_tx.send(BackendEvent::Sessions(sessions));
                            }
                        }
                        Err(error) => {
                            let _ = event_tx.send(BackendEvent::Error(error));
                        }
                    }
                }
                Some(BackendCommand::Resume(id)) => {
                    let Some(owned) = client.as_mut() else {
                        continue;
                    };
                    match owned.resume(id).await {
                        Ok(()) => {
                            let _ = event_tx
                                .send(BackendEvent::Resumed(owned.session_id().to_string()));
                            if let Ok(sessions) = owned.list_session_summaries().await {
                                let _ = event_tx.send(BackendEvent::Sessions(sessions));
                            }
                        }
                        Err(error) => {
                            let _ = event_tx.send(BackendEvent::Error(error));
                        }
                    }
                }
                Some(BackendCommand::RenameSession { session_id, title }) => {
                    let Some(owned) = client.as_mut() else {
                        continue;
                    };
                    match owned.rename_session(&session_id, &title).await {
                        Ok(summary) => {
                            let _ = event_tx.send(BackendEvent::SessionRenamed(summary));
                        }
                        Err(error) => {
                            let _ = event_tx.send(BackendEvent::Error(error));
                        }
                    }
                }
                Some(BackendCommand::DeleteSession(session_id)) => {
                    let Some(owned) = client.as_mut() else {
                        continue;
                    };
                    match owned.delete_session(&session_id).await {
                        Ok(replacement_session_id) => match owned.list_session_summaries().await {
                            Ok(sessions) => {
                                let _ = event_tx.send(BackendEvent::SessionDeleted {
                                    session_id,
                                    replacement_session_id,
                                    sessions,
                                });
                            }
                            Err(error) => {
                                let _ = event_tx.send(BackendEvent::Error(error));
                            }
                        },
                        Err(error) => {
                            let _ = event_tx.send(BackendEvent::Error(error));
                        }
                    }
                }
                Some(BackendCommand::Shutdown) | None => break,
            }
        }
    });
    (handle, event_rx)
}
