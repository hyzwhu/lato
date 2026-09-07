// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/agent_runner.rs
// License: Apache-2.0
// Lato changes: executes each task in a provider-neutral RuntimeSession with a narrowed Lato tool runtime

use super::events::relay_child_events;
use super::{
    BuiltinProfileName, ChildMessage, ChildSessionControl, ContextPackageBuilder,
    ContextPackageLimits, ContextReference,
};
use crate::{ChildSessionConfig, HistoryItem, RuntimePromptOutcome, RuntimeSession, ToolApproval};
use lato_ai::ModelStream;
use lato_core::{
    AgentProfile, BudgetAmount, TaskError, TaskErrorCode, TaskProgress, TaskResult, TaskUsage,
};
use lato_runtime::{
    ChannelBackend, StartedTask, TaskCompletion, TaskReporter, TaskRunOutput, TaskRunRequest,
    TaskRunner,
};
use lato_tools::{BuiltinToolEnvironment, builtin_tool_runtime_for_capabilities_with_subagents};
use lato_workspace::{FileLocks, SandboxProfile, SessionTrust, WorkspaceMode};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

pub struct ChildSessionRunner {
    stream: Arc<dyn ModelStream>,
    locks: Arc<FileLocks>,
    trust: SessionTrust,
    updates: mpsc::UnboundedSender<serde_json::Value>,
    approval: Option<Arc<dyn ToolApproval>>,
    context_limits: ContextPackageLimits,
    message_capacity: usize,
    shutdown_timeout: Duration,
}

impl ChildSessionRunner {
    pub fn new(
        stream: Arc<dyn ModelStream>,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        updates: mpsc::UnboundedSender<serde_json::Value>,
        approval: Option<Arc<dyn ToolApproval>>,
    ) -> Self {
        Self {
            stream,
            locks,
            trust,
            updates,
            approval,
            context_limits: ContextPackageLimits::default(),
            message_capacity: 8,
            shutdown_timeout: Duration::from_secs(5),
        }
    }

    pub fn with_limits(
        mut self,
        context_limits: ContextPackageLimits,
        message_capacity: usize,
        shutdown_timeout: Duration,
    ) -> Self {
        self.context_limits = context_limits;
        self.message_capacity = message_capacity.max(1);
        self.shutdown_timeout = shutdown_timeout;
        self
    }

    fn child_trust(&self, request: &TaskRunRequest) -> SessionTrust {
        let mut trust = self.trust.clone();
        trust.sandbox = match request.workspace_lease.mode {
            WorkspaceMode::SharedReadOnly => SandboxProfile::ReadOnly,
            WorkspaceMode::IsolatedWorktree | WorkspaceMode::SharedSerializedWrite => {
                SandboxProfile::Workspace
            }
            WorkspaceMode::ExternalLease => SandboxProfile::ReadOnly,
        };
        trust
    }

    async fn execute(
        &self,
        request: TaskRunRequest,
        reporter: TaskReporter<ChildSessionControl>,
    ) -> Result<(String, TaskUsage), TaskError> {
        let trust = self.child_trust(&request);
        let tool_runtime = builtin_tool_runtime_for_capabilities_with_subagents(
            BuiltinToolEnvironment {
                cwd: request.workspace_lease.root.clone(),
                locks: Arc::clone(&self.locks),
                trust: trust.clone(),
            },
            Some(&request.node.permissions),
            ChannelBackend::new(request.scoped_handle.clone()).into_resource(),
        )
        .map_err(|error| task_error(TaskErrorCode::RunnerInitialization, error))?;

        let references = request
            .node
            .scope
            .context_refs
            .iter()
            .enumerate()
            .map(|(index, reference)| ContextReference {
                id: format!("context-{index}"),
                summary: reference.clone(),
                location: None,
            })
            .collect();
        let package = ContextPackageBuilder::new(self.context_limits)
            .task(request.node.scope.objective.clone())
            .profile_instructions(request.node.profile.instructions.clone())
            .constraints(vec![
                "Use only the tools and workspace access exposed to this child session.".into(),
                "Return only evidence and artifacts produced within this delegated task.".into(),
                result_contract_instruction(&request.node.profile),
            ])
            .references(references)
            .workspace_root(request.workspace_lease.root.clone())
            .remaining_budget(BudgetAmount::ZERO)
            .build()?;

        let (child_updates_tx, child_updates_rx) = mpsc::unbounded_channel();
        let session = Arc::new(
            RuntimeSession::new_child(ChildSessionConfig {
                session_id: request.node.id.to_string(),
                stream: Arc::clone(&self.stream),
                locks: Arc::clone(&self.locks),
                trust,
                cwd: request.workspace_lease.root.clone(),
                updates: child_updates_tx,
                approval: self.approval.clone(),
                tool_runtime,
                initial_history: vec![HistoryItem::System(package.render())],
            })
            .await
            .map_err(|error| task_error(TaskErrorCode::RunnerInitialization, error))?,
        );

        let progress = Arc::new(Mutex::new(TaskProgress {
            phase: Some("running".into()),
            message: Some("child runtime started".into()),
            completed_units: 0,
            total_units: None,
        }));
        let (messages_tx, mut messages_rx) = mpsc::channel::<ChildMessage>(self.message_capacity);
        let control = Arc::new(ChildSessionControl::new(
            Arc::clone(&session),
            request.cancellation.clone(),
            messages_tx,
            Arc::clone(&progress),
        ));
        if !reporter
            .started(StartedTask::new(control, request.cancellation.clone()))
            .await
        {
            let _ = session.cancel_and_join(self.shutdown_timeout).await;
            return Err(TaskError::new(
                TaskErrorCode::Cancelled,
                "task promotion was rejected by the coordinator",
            ));
        }

        let relay = tokio::spawn(relay_child_events(
            child_updates_rx,
            self.updates.clone(),
            reporter.clone(),
            progress,
        ));
        let message_session = Arc::clone(&session);
        let message_dispatch = tokio::spawn(async move {
            while let Some(message) = messages_rx.recv().await {
                let _ = message_session.steer(message.text).await;
            }
        });

        let outcome = tokio::select! {
            biased;
            _ = request.cancellation.cancelled() => {
                let _ = session.cancel().await;
                Err(TaskError::new(TaskErrorCode::Cancelled, "child task was cancelled"))
            }
            outcome = session.prompt(request.node.scope.objective.clone()) => {
                match outcome {
                    Ok(RuntimePromptOutcome::Complete { text }) => Ok(text),
                    Ok(RuntimePromptOutcome::Cancelled { .. }) => Err(TaskError::new(
                        TaskErrorCode::Cancelled,
                        "child runtime cancelled the delegated turn",
                    )),
                    Err(error) => Err(task_error(TaskErrorCode::RunnerProtocolViolation, error)),
                }
            }
        };

        let shutdown = session.cancel_and_join(self.shutdown_timeout).await;
        message_dispatch.abort();
        let _ = message_dispatch.await;
        relay.abort();
        let _ = relay.await;
        if let Err(error) = shutdown {
            return Err(task_error(TaskErrorCode::RunnerProtocolViolation, error));
        }
        outcome.map(|text| (text, TaskUsage::default()))
    }
}

fn result_contract_instruction(profile: &AgentProfile) -> String {
    match profile.name.as_str() {
        "explorer" => "Return exactly one JSON object: {\"answer\":string,\"evidence\":[{\"id\":string,\"summary\":string,\"location\":string}],\"citations\":[string]}. Every citation must name an evidence id.".into(),
        "worker" => "Return exactly one JSON object: {\"summary\":string,\"changed_files\":[relative_path],\"tests\":[{\"command\":string,\"passed\":boolean,\"summary\":string}],\"artifacts\":[relative_path]}.".into(),
        "reviewer" => "Return exactly one JSON object: {\"summary\":string,\"findings\":[{\"severity\":\"critical\"|\"major\"|\"minor\",\"message\":string,\"evidence\":string,\"file\":relative_path|null,\"line\":positive_integer|null}]} .".into(),
        _ => "Return one bounded JSON object matching the declared result contract.".into(),
    }
}

#[async_trait::async_trait]
impl TaskRunner for ChildSessionRunner {
    type Control = ChildSessionControl;

    async fn run(
        &self,
        request: TaskRunRequest,
        reporter: TaskReporter<Self::Control>,
    ) -> TaskRunOutput {
        let started = Instant::now();
        match self.execute(request, reporter).await {
            Ok((output, usage)) => TaskResult {
                success: true,
                output,
                error: None,
                usage,
                duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                output_ref: None,
            }
            .into(),
            Err(error) => TaskResult {
                success: false,
                output: String::new(),
                error: Some(error.clone()),
                usage: TaskUsage::default(),
                duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                output_ref: None,
            }
            .into(),
        }
    }

    async fn validate_profile(&self, profile: &AgentProfile) -> Result<(), TaskError> {
        let name = BuiltinProfileName::try_from(profile.name.as_str())?;
        if name.resolve() == *profile {
            Ok(())
        } else {
            Err(TaskError::new(
                TaskErrorCode::InvalidProfile,
                "built-in task profiles cannot be overridden",
            ))
        }
    }

    fn on_completed(&self, _completion: TaskCompletion) {}
}

fn task_error(code: TaskErrorCode, error: impl std::fmt::Display) -> TaskError {
    TaskError::new(code, error.to_string())
}
