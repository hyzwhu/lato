use crate::client::{
    ClientUpdate, CompactionResponse, InteractiveAcpClient, ModelSwitchResponse, SessionSummary,
};
use lato_agent::{ApprovalRequest, ToolApproval};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanAction {
    Status,
    Enter,
    Submit,
    Approve,
    Exit,
}

#[derive(Debug)]
pub enum BackendCommand {
    Submit(String),
    InvokeSkill {
        name: String,
        args: Option<String>,
        context: Option<String>,
    },
    ListSkills,
    ListWorkflows,
    LaunchWorkflow {
        id: String,
        args: serde_json::Value,
        agent_budget: Option<u64>,
    },
    WorkflowRuns,
    WorkflowPause(String),
    WorkflowResume {
        name: String,
        agent_budget: Option<u64>,
    },
    WorkflowStop(String),
    Compact(Option<String>),
    SwitchModel(String),
    Cancel,
    NewSession,
    Resume(String),
    RenameSession {
        session_id: String,
        title: String,
    },
    DeleteSession(String),
    Plan(PlanAction),
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendEvent {
    SessionReady(String),
    Skills(crate::client::SkillListResponse),
    SkillsError(String),
    Workflows(crate::client::WorkflowListResponse),
    WorkflowsError(String),
    WorkflowLaunched(Result<crate::client::WorkflowRunView, String>),
    WorkflowRuns(Vec<crate::client::WorkflowRunView>),
    WorkflowAction(Result<crate::client::WorkflowRunView, String>),
    Update(ClientUpdate),
    TurnCompleted(String),
    TurnCancelled,
    NewSessionCreated(String),
    Resumed(String),
    Sessions(Vec<SessionSummary>),
    SessionRenamed(SessionSummary),
    ModelSwitched(ModelSwitchResponse),
    SessionDeleted {
        session_id: String,
        replacement_session_id: Option<String>,
        sessions: Vec<SessionSummary>,
    },
    PlanResult(Result<String, String>),
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

type ActiveWork = tokio::task::JoinHandle<(InteractiveAcpClient, ActiveWorkEnd)>;

#[cfg(test)]
impl BackendHandle {
    pub(crate) fn test_handle() -> (Self, tokio::sync::mpsc::UnboundedReceiver<BackendCommand>) {
        let (commands, receiver) = mpsc::unbounded_channel();
        (BackendHandle { commands }, receiver)
    }
}

#[derive(Debug)]
enum ActiveWorkEnd {
    Turn(TurnEnd),
    Compaction(Result<CompactionResponse, String>),
}

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
        let mut active: Option<ActiveWork> = None;
        let mut cancel: Option<oneshot::Sender<()>> = None;
        if let Some(session) = client.as_ref().map(InteractiveAcpClient::session_id) {
            let _ = event_tx.send(BackendEvent::SessionReady(session.to_string()));
        }

        if let Some(owned) = client.as_mut() {
            refresh_skills(owned, &event_tx).await;
        }
        // Poll out-of-turn notifications (background `lato/workflow` updates).
        let mut update_tick = tokio::time::interval(std::time::Duration::from_millis(250));
        update_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
                        Some(BackendCommand::Submit(_) | BackendCommand::InvokeSkill { .. } | BackendCommand::Compact(_) | BackendCommand::SwitchModel(_)) => {
                            let _ = event_tx.send(BackendEvent::Error("session work is already running".into()));
                        }
                        Some(BackendCommand::ListSkills) => {
                            let _ = event_tx.send(BackendEvent::SkillsError("Wait for the current turn before refreshing skills / 请等待当前回复结束后刷新技能".into()));
                        }
                        Some(BackendCommand::ListWorkflows) => {
                            let _ = event_tx.send(BackendEvent::WorkflowsError("Wait for the current turn before listing workflows / 请等待当前回复结束后查看工作流".into()));
                        }
                        Some(command @ (BackendCommand::LaunchWorkflow { .. }
                        | BackendCommand::WorkflowRuns
                        | BackendCommand::WorkflowPause(_)
                        | BackendCommand::WorkflowResume { .. }
                        | BackendCommand::WorkflowStop(_))) => {
                            // The ACP client is owned by the active turn task;
                            // workflow commands need it, so queue a clear error
                            // instead of silently dropping the request.
                            let _ = event_tx.send(match command {
                                BackendCommand::WorkflowRuns => {
                                    BackendEvent::Error("wait for the current turn before listing workflow runs / 请等待当前回复结束后查看运行".into())
                                }
                                _ => BackendEvent::Error("wait for the current turn before managing workflows / 请等待当前回复结束后操作工作流".into()),
                            });
                        }
                        Some(BackendCommand::Resume(_)) => {
                            let _ = event_tx.send(BackendEvent::Error("cannot switch sessions while a turn is running".into()));
                        }
                        Some(BackendCommand::RenameSession { .. } | BackendCommand::DeleteSession(_)) => {
                            let _ = event_tx.send(BackendEvent::Error("cannot change sessions while a turn is running".into()));
                        }
                        Some(BackendCommand::NewSession) => {
                            let _ = event_tx.send(BackendEvent::Error("cannot clear while a turn is running".into()));
                        }
                        Some(BackendCommand::Plan(_)) => {
                            let _ = event_tx.send(BackendEvent::PlanResult(Err(
                                "wait for the current turn before plan commands / 请等待当前回复结束后操作 Plan mode".into(),
                            )));
                        }
                    },
                    result = turn => {
                        match result {
                            Ok((mut returned, ActiveWorkEnd::Turn(TurnEnd::Completed(text)))) => {
                                let _ = event_tx.send(BackendEvent::TurnCompleted(text));
                                if let Ok(sessions) = returned.list_session_summaries().await {
                                    let _ = event_tx.send(BackendEvent::Sessions(sessions));
                                }
                                refresh_skills(&mut returned, &event_tx).await;
                                client = Some(returned);
                            }
                            Ok((returned, ActiveWorkEnd::Turn(TurnEnd::Cancelled(cancelled)))) => {
                                client = Some(returned);
                                match cancelled {
                                    Ok(()) => { let _ = event_tx.send(BackendEvent::TurnCancelled); }
                                    Err(error) => { let _ = event_tx.send(BackendEvent::Error(error)); }
                                }
                            }
                            Ok((returned, ActiveWorkEnd::Turn(TurnEnd::Failed(error)))) => {
                                client = Some(returned);
                                let _ = event_tx.send(BackendEvent::Error(error));
                            }
                            Ok((returned, ActiveWorkEnd::Compaction(result))) => {
                                client = Some(returned);
                                if let Err(error) = result {
                                    let _ = event_tx.send(BackendEvent::Error(error));
                                }
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

            let received = tokio::select! {
                command = command_rx.recv() => command,
                _ = update_tick.tick() => {
                    if let Some(owned) = client.as_mut() {
                        for update in owned.drain_updates() {
                            let _ = event_tx.send(BackendEvent::Update(update));
                        }
                    }
                    continue;
                }
            };
            match received {
                Some(BackendCommand::ListSkills) => {
                    if let Some(owned) = client.as_mut() {
                        refresh_skills(owned, &event_tx).await;
                    }
                }
                Some(BackendCommand::ListWorkflows) => {
                    if let Some(owned) = client.as_mut() {
                        refresh_workflows(owned, &event_tx).await;
                    }
                }
                Some(
                    command @ (BackendCommand::LaunchWorkflow { .. }
                    | BackendCommand::WorkflowRuns
                    | BackendCommand::WorkflowPause(_)
                    | BackendCommand::WorkflowResume { .. }
                    | BackendCommand::WorkflowStop(_)),
                ) => {
                    handle_workflow_command(command, &mut client, &event_tx).await;
                }
                Some(
                    command @ (BackendCommand::Submit(_) | BackendCommand::InvokeSkill { .. }),
                ) => {
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
                            let callback = |raw: &serde_json::Value| {
                                let update = ClientUpdate::from_json(raw);
                                if update != ClientUpdate::Unknown {
                                    let _ = updates.send(BackendEvent::Update(update));
                                }
                            };
                            let send = async {
                                match command {
                                    BackendCommand::Submit(text) => {
                                        owned.send_streaming(text, callback).await
                                    }
                                    BackendCommand::InvokeSkill {
                                        name,
                                        args,
                                        context,
                                    } => {
                                        if context.is_some() {
                                            owned
                                                .send_skill_streaming_with_context(
                                                    name, args, context, callback,
                                                )
                                                .await
                                        } else {
                                            owned.send_skill_streaming(name, args, callback).await
                                        }
                                    }
                                    _ => unreachable!(),
                                }
                            };
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
                            (owned, ActiveWorkEnd::Turn(TurnEnd::Cancelled(result)))
                        } else {
                            let result = response.expect("completed branch sets response");
                            match result {
                                Ok(text) => (owned, ActiveWorkEnd::Turn(TurnEnd::Completed(text))),
                                Err(error) => (owned, ActiveWorkEnd::Turn(TurnEnd::Failed(error))),
                            }
                        }
                    }));
                }
                Some(BackendCommand::Compact(user_context)) => {
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
                            let compact = owned.compact_streaming(user_context, |raw| {
                                let update = ClientUpdate::from_json(raw);
                                if update != ClientUpdate::Unknown {
                                    let _ = updates.send(BackendEvent::Update(update));
                                }
                            });
                            tokio::pin!(compact);
                            tokio::select! {
                                result = &mut compact => {
                                    response = Some(result);
                                    cancelled = false;
                                }
                                _ = cancel_rx => {
                                    response = None;
                                    cancelled = true;
                                }
                            }
                        }
                        let result = if cancelled {
                            owned.cancel().await.map(|_| {
                                let _ = updates
                                    .send(BackendEvent::Update(ClientUpdate::CompactionCancelled));
                                CompactionResponse {
                                    status: "cancelled".into(),
                                    before: None,
                                    after: None,
                                    checkpoint_id: None,
                                    warning: None,
                                }
                            })
                        } else {
                            response.expect("completed branch sets response")
                        };
                        (owned, ActiveWorkEnd::Compaction(result))
                    }));
                }
                Some(BackendCommand::SwitchModel(selection)) => {
                    let Some(owned) = client.as_mut() else {
                        let _ = event_tx.send(BackendEvent::Error("session is unavailable".into()));
                        continue;
                    };
                    match owned.set_model(&selection).await {
                        Ok(response) => {
                            let _ = event_tx.send(BackendEvent::ModelSwitched(response));
                        }
                        Err(error) => {
                            let _ = event_tx.send(BackendEvent::Error(error));
                        }
                    }
                }
                Some(BackendCommand::Cancel) => {
                    let _ = event_tx.send(BackendEvent::TurnCancelled);
                }
                Some(BackendCommand::NewSession) => {
                    let Some(owned) = client.as_mut() else {
                        let _ = event_tx.send(BackendEvent::Error("session is unavailable".into()));
                        continue;
                    };
                    match owned.clear().await {
                        Ok(()) => {
                            refresh_skills(owned, &event_tx).await;
                            let _ = event_tx.send(BackendEvent::NewSessionCreated(
                                owned.session_id().to_string(),
                            ));
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
                            refresh_skills(owned, &event_tx).await;
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
                Some(BackendCommand::Plan(action)) => {
                    let Some(owned) = client.as_mut() else {
                        let _ = event_tx.send(BackendEvent::PlanResult(Err(
                            "session is unavailable".into(),
                        )));
                        continue;
                    };
                    let result = match plan_request(owned, action).await {
                        Ok(status) => BackendEvent::PlanResult(Ok(format_plan_status(&status))),
                        Err(error) => BackendEvent::PlanResult(Err(error)),
                    };
                    let _ = event_tx.send(result);
                }
                Some(BackendCommand::Shutdown) | None => break,
            }
        }
    });
    (handle, event_rx)
}

async fn plan_request(
    client: &mut InteractiveAcpClient,
    action: PlanAction,
) -> Result<serde_json::Value, String> {
    let action_name = match action {
        PlanAction::Status => "status",
        PlanAction::Enter => "enter",
        PlanAction::Submit => "submit",
        PlanAction::Approve => "approve",
        PlanAction::Exit => "exit",
    };
    client
        .plan_request(action_name, serde_json::json!({}))
        .await
}

fn format_plan_status(status: &serde_json::Value) -> String {
    let phase = status["phase"].as_str().unwrap_or("unknown");
    let path = status["planPath"].as_str().unwrap_or("plan.md");
    let approval = status["approval"].as_object();
    let approval_text = approval
        .map(|approval| {
            format!(
                " · approved generation {} by {}",
                approval["generation"].as_u64().unwrap_or_default(),
                approval["approver"].as_str().unwrap_or("user")
            )
        })
        .unwrap_or_default();
    format!("[Plan mode] {phase} · {path}{approval_text}")
}

async fn refresh_skills(
    client: &mut InteractiveAcpClient,
    events: &mpsc::UnboundedSender<BackendEvent>,
) {
    let event = match client.list_skills().await {
        Ok(response) => BackendEvent::Skills(response),
        Err(error) => BackendEvent::SkillsError(error),
    };
    let _ = events.send(event);
}

async fn refresh_workflows(
    client: &mut InteractiveAcpClient,
    events: &mpsc::UnboundedSender<BackendEvent>,
) {
    let event = match client.list_workflows().await {
        Ok(response) => BackendEvent::Workflows(response),
        Err(error) => BackendEvent::WorkflowsError(error),
    };
    let _ = events.send(event);
}

async fn handle_workflow_command(
    command: BackendCommand,
    client: &mut Option<InteractiveAcpClient>,
    event_tx: &mpsc::UnboundedSender<BackendEvent>,
) {
    let Some(owned) = client.as_mut() else {
        let _ = event_tx.send(BackendEvent::Error("session is unavailable".into()));
        return;
    };
    match command {
        BackendCommand::LaunchWorkflow {
            id,
            args,
            agent_budget,
        } => {
            let _ = event_tx.send(BackendEvent::WorkflowLaunched(
                owned.launch_workflow(&id, args, agent_budget).await,
            ));
        }
        BackendCommand::WorkflowRuns => {
            let _ = event_tx.send(BackendEvent::WorkflowRuns(
                match owned.list_workflow_runs().await {
                    Ok(runs) => runs,
                    Err(error) => {
                        let _ = event_tx.send(BackendEvent::Error(error));
                        return;
                    }
                },
            ));
        }
        BackendCommand::WorkflowPause(name) => {
            let _ = event_tx.send(BackendEvent::WorkflowAction(
                owned.pause_workflow(&name).await,
            ));
        }
        BackendCommand::WorkflowResume { name, agent_budget } => {
            let _ = event_tx.send(BackendEvent::WorkflowAction(
                owned.resume_workflow(&name, agent_budget).await,
            ));
        }
        BackendCommand::WorkflowStop(name) => {
            let _ = event_tx.send(BackendEvent::WorkflowAction(
                owned.stop_workflow(&name).await,
            ));
        }
        _ => unreachable!("handle_workflow_command only receives workflow commands"),
    }
}
