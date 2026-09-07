// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/mod.rs
// License: Apache-2.0
// Lato changes: exposes the bounded coordinator as five small provider-neutral lifecycle tools

use async_trait::async_trait;
use lato_core::{
    AgentProfile, BudgetAmount, BudgetLimits, ResultContract, Retryability, SideEffect, TaskError,
    TaskId, TaskScope, Tool, ToolCancellation, ToolCapability, ToolConcurrency, ToolContext,
    ToolDescriptor, ToolError, ToolIdempotency, ToolLayer, ToolName, ToolOutput, ToolSource,
};
use lato_runtime::{
    ActiveMessageOperation, ActiveMessageOutcome, ActiveMessageRequest, SpawnMode,
    SpawnTaskRequest, SubagentBackendResource, TaskSnapshot, WaitOutcome,
};
use semver::Version;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const MAX_OBJECTIVE_BYTES: usize = 32 * 1024;
const MAX_CONTEXT_REFS: usize = 32;
const MAX_CONTEXT_REF_BYTES: usize = 2 * 1024;
const MAX_WAIT_MS: u64 = 3_600_000;
const DEFAULT_FOREGROUND_WAIT_MS: u64 = 45_000;

#[derive(Clone, Copy)]
enum TaskToolKind {
    Spawn,
    Send,
    Wait,
    Cancel,
    Inspect,
}

pub fn task_tool_definitions() -> Vec<Value> {
    vec![
        definition(
            "spawn",
            "Start a bounded child task using a built-in profile.",
            json!({
                "type": "object",
                "properties": {
                    "task_id": {"type":"string", "maxLength":256},
                    "profile": {"type":"string", "enum":["explorer", "worker", "reviewer"]},
                    "task": {"type":"string", "maxLength":MAX_OBJECTIVE_BYTES},
                    "context_refs": {"type":"array", "maxItems":MAX_CONTEXT_REFS, "items":{"type":"string", "maxLength":MAX_CONTEXT_REF_BYTES}},
                    "background": {"type":"boolean", "default":true},
                    "timeout_ms": {"type":"integer", "minimum":1, "maximum":MAX_WAIT_MS}
                },
                "required": ["profile", "task"]
            }),
        ),
        definition(
            "send",
            "Send a queued message or steering instruction to an active child task.",
            json!({
                "type":"object",
                "properties": {
                    "task_id":{"type":"string"},
                    "message":{"type":"string", "maxLength":32768},
                    "steer":{"type":"boolean", "default":false}
                },
                "required":["task_id", "message"]
            }),
        ),
        definition(
            "wait",
            "Wait for a child task to finish, with a bounded timeout.",
            json!({
                "type":"object",
                "properties": {
                    "task_id":{"type":"string"},
                    "timeout_ms":{"type":"integer", "minimum":1, "maximum":MAX_WAIT_MS}
                },
                "required":["task_id"]
            }),
        ),
        definition(
            "cancel",
            "Cancel a child task and all of its descendants.",
            json!({
                "type":"object",
                "properties":{"task_id":{"type":"string"}},
                "required":["task_id"]
            }),
        ),
        definition(
            "inspect",
            "Inspect one child task, or list all running child tasks.",
            json!({
                "type":"object",
                "properties":{"task_id":{"type":"string"}}
            }),
        ),
    ]
}

fn definition(name: &str, description: &str, parameters: Value) -> Value {
    json!({"type":"function", "function":{"name":name, "description":description, "parameters":parameters}})
}

pub fn task_tools(backend: SubagentBackendResource) -> Vec<Arc<dyn Tool>> {
    [
        TaskToolKind::Spawn,
        TaskToolKind::Send,
        TaskToolKind::Wait,
        TaskToolKind::Cancel,
        TaskToolKind::Inspect,
    ]
    .into_iter()
    .map(|kind| {
        Arc::new(TaskLifecycleTool {
            kind,
            backend: backend.clone(),
        }) as Arc<dyn Tool>
    })
    .collect()
}

struct TaskLifecycleTool {
    kind: TaskToolKind,
    backend: SubagentBackendResource,
}

#[async_trait]
impl Tool for TaskLifecycleTool {
    fn descriptor(&self) -> ToolDescriptor {
        let definition = &task_tool_definitions()[self.kind.index()];
        let function = &definition["function"];
        ToolDescriptor {
            name: ToolName::parse(format!("builtin:{}", self.kind.name()))
                .expect("static tool name"),
            version: Version::new(1, 0, 0),
            description: function["description"]
                .as_str()
                .expect("static description")
                .into(),
            input_schema: function["parameters"].clone(),
            capabilities: vec![ToolCapability::TaskControl],
            side_effect: SideEffect::None,
            concurrency: ToolConcurrency::Parallel,
            idempotency: if matches!(self.kind, TaskToolKind::Wait | TaskToolKind::Inspect) {
                ToolIdempotency::Idempotent
            } else {
                ToolIdempotency::NonIdempotent
            },
            timeout_ms: MAX_WAIT_MS + 5_000,
            max_output_bytes: 256 * 1024,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource {
                layer: ToolLayer::Builtin,
                id: format!("lato.builtin.{}", self.kind.name()),
                replacement: None,
            },
        }
    }

    async fn invoke(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(ToolError::new(
                "tool.cancelled",
                "tool call was cancelled",
                Retryability::Never,
            ));
        }
        let value = match self.kind {
            TaskToolKind::Spawn => self.spawn(context, arguments).await?,
            TaskToolKind::Send => self.send(arguments).await?,
            TaskToolKind::Wait => self.wait(arguments).await?,
            TaskToolKind::Cancel => self.cancel(arguments).await?,
            TaskToolKind::Inspect => self.inspect(arguments).await?,
        };
        Ok(ToolOutput {
            content: value.to_string(),
            metadata: json!({"taskLifecycle": self.kind.name()}),
            truncated: false,
            artifact_path: None,
        })
    }
}

impl TaskLifecycleTool {
    async fn spawn(&self, context: ToolContext, arguments: Value) -> Result<Value, ToolError> {
        let input: SpawnInput = parse(arguments)?;
        validate_bounded_text("task", &input.task, MAX_OBJECTIVE_BYTES)?;
        if input.context_refs.len() > MAX_CONTEXT_REFS {
            return Err(invalid_arguments("too many context_refs"));
        }
        for reference in &input.context_refs {
            validate_bounded_text("context_ref", reference, MAX_CONTEXT_REF_BYTES)?;
        }
        self.backend
            .backend()
            .validate_profile(&input.profile)
            .await
            .map_err(task_error)?;
        let profile = match input.profile.as_str() {
            "explorer" => AgentProfile::explorer(),
            "worker" => AgentProfile::worker(),
            "reviewer" => AgentProfile::reviewer(),
            _ => {
                return Err(invalid_arguments(
                    "profile must be explorer, worker, or reviewer",
                ));
            }
        };
        let task_id = match input.task_id {
            Some(task_id) => {
                TaskId::parse(task_id).map_err(|error| invalid_arguments(&error.to_string()))?
            }
            None => TaskId::from(format!("task-{}-{}", context.session_id, context.call_id)),
        };
        let background = input.background.unwrap_or(true);
        let disposition = self
            .backend
            .backend()
            .spawn(SpawnTaskRequest {
                task_id: task_id.clone(),
                scope: TaskScope {
                    objective: input.task,
                    context_refs: input.context_refs,
                },
                profile,
                requested_capabilities: None,
                budget: child_budget(),
                result_contract: ResultContract {
                    schema: None,
                    max_output_bytes: 256 * 1024,
                },
                mode: if background {
                    SpawnMode::Background
                } else {
                    SpawnMode::AwaitCompletion
                },
                cancellation: context.cancellation,
            })
            .await
            .map_err(task_error)?;
        if background {
            return Ok(json!({"task_id":task_id, "status":disposition.status, "background":true}));
        }
        let timeout = bounded_timeout(input.timeout_ms.unwrap_or(DEFAULT_FOREGROUND_WAIT_MS))?;
        wait_value(
            self.backend
                .backend()
                .wait(task_id, timeout)
                .await
                .map_err(task_error)?,
        )
    }

    async fn send(&self, arguments: Value) -> Result<Value, ToolError> {
        let input: SendInput = parse(arguments)?;
        let task_id = parse_task_id(input.task_id)?;
        let operation = if input.steer {
            ActiveMessageOperation::Steer
        } else {
            ActiveMessageOperation::Queue
        };
        let request =
            ActiveMessageRequest::try_new_with_operation(task_id, input.message, operation)
                .map_err(active_message_error)?;
        active_message_value(self.backend.backend().send_active_message(request).await)
    }

    async fn wait(&self, arguments: Value) -> Result<Value, ToolError> {
        let input: WaitInput = parse(arguments)?;
        let timeout = bounded_timeout(input.timeout_ms.unwrap_or(DEFAULT_FOREGROUND_WAIT_MS))?;
        wait_value(
            self.backend
                .backend()
                .wait(parse_task_id(input.task_id)?, timeout)
                .await
                .map_err(task_error)?,
        )
    }

    async fn cancel(&self, arguments: Value) -> Result<Value, ToolError> {
        let input: TaskIdInput = parse(arguments)?;
        let outcome = self
            .backend
            .backend()
            .cancel(parse_task_id(input.task_id)?)
            .await
            .map_err(task_error)?;
        Ok(serde_json::to_value(outcome).expect("cancel outcome serializes"))
    }

    async fn inspect(&self, arguments: Value) -> Result<Value, ToolError> {
        let input: InspectInput = parse(arguments)?;
        if let Some(task_id) = input.task_id {
            return Ok(snapshot_value(
                &self
                    .backend
                    .backend()
                    .inspect(parse_task_id(task_id)?)
                    .await
                    .map_err(task_error)?,
            ));
        }
        let snapshots = self
            .backend
            .backend()
            .list_running()
            .await
            .map_err(task_error)?;
        Ok(Value::Array(snapshots.iter().map(snapshot_value).collect()))
    }
}

impl TaskToolKind {
    const fn index(self) -> usize {
        match self {
            Self::Spawn => 0,
            Self::Send => 1,
            Self::Wait => 2,
            Self::Cancel => 3,
            Self::Inspect => 4,
        }
    }
    const fn name(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Send => "send",
            Self::Wait => "wait",
            Self::Cancel => "cancel",
            Self::Inspect => "inspect",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnInput {
    #[serde(default)]
    task_id: Option<String>,
    profile: String,
    #[serde(alias = "objective")]
    task: String,
    #[serde(default)]
    context_refs: Vec<String>,
    #[serde(default)]
    background: Option<bool>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendInput {
    task_id: String,
    message: String,
    #[serde(default)]
    steer: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitInput {
    task_id: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskIdInput {
    task_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectInput {
    #[serde(default)]
    task_id: Option<String>,
}

fn child_budget() -> BudgetLimits {
    BudgetLimits::limited(BudgetAmount {
        input_tokens: 500_000,
        output_tokens: 100_000,
        total_tokens: 600_000,
        tool_calls: 512,
        cost_micros: 10_000_000,
        wall_time_ms: 1_800_000,
        retries: 16,
        child_tasks: 16,
        worktrees: 16,
    })
}

fn snapshot_value(snapshot: &TaskSnapshot) -> Value {
    json!({
        "task": snapshot.node,
        "budget_limits": snapshot.budget_limits,
        "budget_spent": snapshot.budget_spent,
        "budget_reserved": snapshot.budget_reserved,
        "workspace": snapshot.workspace_lease,
        "elapsed_ms": snapshot.elapsed_ms,
        "progress": snapshot.progress,
        "usage": snapshot.usage,
        "result": snapshot.result,
        "event_sequence": snapshot.event_sequence,
        "completion": snapshot.completion_disposition,
        "output_metadata": snapshot.output_metadata,
        "cleanup_error": snapshot.cleanup_error,
    })
}

fn wait_value(outcome: WaitOutcome) -> Result<Value, ToolError> {
    match outcome {
        WaitOutcome::Finished(snapshot) => {
            Ok(json!({"wait":"finished", "snapshot":snapshot_value(&snapshot)}))
        }
        WaitOutcome::TimedOut(snapshot) => {
            Ok(json!({"wait":"timed_out", "snapshot":snapshot_value(&snapshot)}))
        }
        WaitOutcome::NotFoundOrNotOwned => Err(ToolError::new(
            "task.not_found_or_not_owned",
            "task was not found in this session scope",
            Retryability::Never,
        )),
    }
}

fn active_message_value(outcome: ActiveMessageOutcome) -> Result<Value, ToolError> {
    match outcome {
        ActiveMessageOutcome::Accepted { message_id } => {
            Ok(json!({"accepted":true, "message_id":message_id}))
        }
        ActiveMessageOutcome::Limit {
            max_bytes,
            observed_bytes,
        } => Err(invalid_arguments(&format!(
            "message is {observed_bytes} bytes; maximum is {max_bytes}"
        ))),
        ActiveMessageOutcome::NotFoundOrNotOwned => Err(ToolError::new(
            "task.not_found_or_not_owned",
            "task was not found in this session scope",
            Retryability::Never,
        )),
        ActiveMessageOutcome::NotActiveOrFinalizing => Err(ToolError::new(
            "task.active_message_inactive",
            "task is not active",
            Retryability::Never,
        )),
        ActiveMessageOutcome::Saturated { max_in_flight } => Err(ToolError::new(
            "task.limit.message",
            format!("active message limit reached ({max_in_flight})"),
            Retryability::AfterBackoff,
        )),
        ActiveMessageOutcome::AdmissionUncertain => Err(ToolError::new(
            "task.active_message_uncertain",
            "message admission is uncertain",
            Retryability::RequiresDecision,
        )),
        ActiveMessageOutcome::NotAcceptedBeforeDeadline => Err(ToolError::new(
            "task.active_message_timeout",
            "message was not accepted before the deadline",
            Retryability::Safe,
        )),
        ActiveMessageOutcome::Unsupported => Err(ToolError::new(
            "task.active_message_unsupported",
            "runner does not support active messages",
            Retryability::Never,
        )),
        ActiveMessageOutcome::ChannelClosed => Err(ToolError::new(
            "task.active_message_channel_closed",
            "active message channel is closed",
            Retryability::Safe,
        )),
        _ => Err(ToolError::new(
            "task.active_message_failed",
            "active message failed",
            Retryability::Safe,
        )),
    }
}

fn active_message_error(outcome: ActiveMessageOutcome) -> ToolError {
    match outcome {
        ActiveMessageOutcome::Limit {
            max_bytes,
            observed_bytes,
        } => invalid_arguments(&format!(
            "message is {observed_bytes} bytes; maximum is {max_bytes}"
        )),
        _ => ToolError::new(
            "task.active_message_failed",
            "active message request is invalid",
            Retryability::Never,
        ),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(arguments: Value) -> Result<T, ToolError> {
    serde_json::from_value(arguments).map_err(|error| invalid_arguments(&error.to_string()))
}

fn parse_task_id(value: String) -> Result<TaskId, ToolError> {
    TaskId::parse(value).map_err(|error| invalid_arguments(&error.to_string()))
}

fn bounded_timeout(milliseconds: u64) -> Result<Duration, ToolError> {
    if milliseconds == 0 || milliseconds > MAX_WAIT_MS {
        return Err(invalid_arguments(
            "timeout_ms must be between 1 and 3600000",
        ));
    }
    Ok(Duration::from_millis(milliseconds))
}

fn validate_bounded_text(field: &str, value: &str, maximum: usize) -> Result<(), ToolError> {
    if value.is_empty() || value.len() > maximum {
        return Err(invalid_arguments(&format!(
            "{field} must contain 1..={maximum} UTF-8 bytes"
        )));
    }
    Ok(())
}

fn task_error(error: TaskError) -> ToolError {
    ToolError::new(
        error.code.as_str(),
        error.message,
        error.code.retryability(),
    )
}

fn invalid_arguments(message: &str) -> ToolError {
    ToolError::new("tool.invalid_arguments", message, Retryability::Never)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lato_core::{SessionId, TaskErrorCode, ToolCallId, TurnId};
    use lato_runtime::{CancelOutcome, SpawnDisposition, SubagentBackend};
    use tokio_util::sync::CancellationToken;

    struct ProbeBackend;

    #[async_trait]
    impl SubagentBackend for ProbeBackend {
        async fn spawn(&self, _request: SpawnTaskRequest) -> Result<SpawnDisposition, TaskError> {
            panic!("invalid profiles must be rejected before spawn")
        }

        async fn send_active_message(
            &self,
            _request: ActiveMessageRequest,
        ) -> ActiveMessageOutcome {
            ActiveMessageOutcome::Accepted { message_id: 7 }
        }

        async fn wait(
            &self,
            _task_id: TaskId,
            _timeout: Duration,
        ) -> Result<WaitOutcome, TaskError> {
            Ok(WaitOutcome::NotFoundOrNotOwned)
        }

        async fn cancel(&self, _task_id: TaskId) -> Result<CancelOutcome, TaskError> {
            Ok(CancelOutcome {
                matched: 1,
                newly_requested: 1,
                already_terminal: 0,
            })
        }

        async fn inspect(&self, _task_id: TaskId) -> Result<TaskSnapshot, TaskError> {
            Err(TaskError::new(TaskErrorCode::NotFoundOrNotOwned, "missing"))
        }

        async fn list_running(&self) -> Result<Vec<TaskSnapshot>, TaskError> {
            Ok(Vec::new())
        }

        async fn teardown_root_and_drain(&self) -> Result<CancelOutcome, TaskError> {
            Ok(CancelOutcome::default())
        }

        async fn validate_profile(&self, profile: &str) -> Result<(), TaskError> {
            if matches!(profile, "explorer" | "worker" | "reviewer") {
                Ok(())
            } else {
                Err(TaskError::new(
                    TaskErrorCode::InvalidProfile,
                    "invalid profile",
                ))
            }
        }
    }

    fn context() -> ToolContext {
        ToolContext {
            session_id: SessionId::from("session"),
            turn_id: TurnId::from("turn"),
            call_id: ToolCallId::from("call"),
            cancellation: CancellationToken::new(),
            execution_grant: None,
        }
    }

    fn tool(name: &str) -> Arc<dyn Tool> {
        task_tools(SubagentBackendResource::new(Arc::new(ProbeBackend)))
            .into_iter()
            .find(|tool| tool.descriptor().name.local_name() == name)
            .unwrap()
    }

    #[tokio::test]
    async fn lifecycle_tools_send_cancel_list_and_reject_custom_profiles() {
        let sent = tool("send")
            .invoke(
                context(),
                json!({"task_id":"child", "message":"focus", "steer":true}),
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&sent.content).unwrap()["message_id"],
            7
        );

        let cancelled = tool("cancel")
            .invoke(context(), json!({"task_id":"child"}))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&cancelled.content).unwrap()["newly_requested"],
            1
        );

        let listed = tool("inspect").invoke(context(), json!({})).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&listed.content).unwrap(),
            json!([])
        );

        let error = tool("spawn")
            .invoke(context(), json!({"profile":"custom", "task":"do work"}))
            .await
            .unwrap_err();
        assert_eq!(error.code, "task.invalid_profile");
    }
}
