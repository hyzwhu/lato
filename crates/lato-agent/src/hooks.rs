use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use lato_extensions::hooks::{
    DefaultHookExecutor, HookEventEnvelope, HookEventName, HookRegistry, HookRunContext,
    HookRunRecord, PostToolUseResult, PreToolUseResult, PromptResult, StopResult,
    dispatch_observer, dispatch_post_tool_use, dispatch_pre_tool_use, dispatch_prompt_submit,
    dispatch_stop,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct SessionHookRuntime {
    registry: Arc<HookRegistry>,
    workspace_root: PathBuf,
    session_id: String,
}

impl SessionHookRuntime {
    pub fn new(registry: Arc<HookRegistry>, workspace_root: PathBuf, session_id: String) -> Self {
        Self {
            registry,
            workspace_root,
            session_id,
        }
    }

    pub fn generation(&self) -> u64 {
        self.registry.generation()
    }

    pub fn timeout_for(&self, hook_id: &str) -> u64 {
        [
            HookEventName::SessionStart,
            HookEventName::SessionEnd,
            HookEventName::UserPromptSubmit,
            HookEventName::PreToolUse,
            HookEventName::PostToolUse,
            HookEventName::Stop,
            HookEventName::PreCompact,
            HookEventName::PostCompact,
        ]
        .into_iter()
        .flat_map(|event| self.registry.handlers(event))
        .find(|spec| spec.id == hook_id)
        .map_or(0, |spec| spec.timeout_ms)
    }

    pub async fn prompt_submit(
        &self,
        turn_id: &str,
        text: &str,
        cancellation: CancellationToken,
    ) -> PromptResult {
        let envelope = self.envelope(
            HookEventName::UserPromptSubmit,
            Some(turn_id),
            serde_json::json!({"prompt": text}),
        );
        let Some((executor, context)) = self.execution(cancellation) else {
            return PromptResult {
                block: None,
                runs: Vec::new(),
            };
        };
        dispatch_prompt_submit(&self.registry, &envelope, &context, &executor).await
    }

    pub async fn pre_tool_use(
        &self,
        turn_id: &str,
        tool_name: &str,
        arguments: Value,
        cancellation: CancellationToken,
    ) -> PreToolUseResult {
        let envelope = self.envelope(
            HookEventName::PreToolUse,
            Some(turn_id),
            serde_json::json!({"toolName": tool_name, "arguments": arguments}),
        );
        let Some((executor, context)) = self.execution(cancellation) else {
            return PreToolUseResult {
                decision: lato_extensions::hooks::HookDecision::Allow,
                updated_input: None,
                additional_context: Vec::new(),
                runs: Vec::new(),
            };
        };
        dispatch_pre_tool_use(&self.registry, &envelope, &context, &executor).await
    }

    pub async fn post_tool_use(
        &self,
        turn_id: &str,
        payload: Value,
        cancellation: CancellationToken,
    ) -> PostToolUseResult {
        let envelope = self.envelope(HookEventName::PostToolUse, Some(turn_id), payload);
        let Some((executor, context)) = self.execution(cancellation) else {
            return PostToolUseResult {
                blocks: Vec::new(),
                additional_context: Vec::new(),
                replacement: None,
                runs: Vec::new(),
            };
        };
        dispatch_post_tool_use(&self.registry, &envelope, &context, &executor).await
    }

    pub async fn stop(
        &self,
        turn_id: &str,
        payload: Value,
        cancellation: CancellationToken,
    ) -> StopResult {
        let envelope = self.envelope(HookEventName::Stop, Some(turn_id), payload);
        let Some((executor, context)) = self.execution(cancellation) else {
            return StopResult {
                blocks: Vec::new(),
                additional_context: Vec::new(),
                prevent_continuation: None,
                runs: Vec::new(),
            };
        };
        dispatch_stop(&self.registry, &envelope, &context, &executor).await
    }

    pub async fn observe(
        &self,
        event: HookEventName,
        turn_id: Option<&str>,
        payload: Value,
        cancellation: CancellationToken,
    ) -> Vec<HookRunRecord> {
        let envelope = self.envelope(event, turn_id, payload);
        let Some((executor, context)) = self.execution(cancellation) else {
            return Vec::new();
        };
        dispatch_observer(&self.registry, event, &envelope, &context, &executor).await
    }

    fn envelope(
        &self,
        event: HookEventName,
        turn_id: Option<&str>,
        payload: Value,
    ) -> HookEventEnvelope {
        HookEventEnvelope::new(
            event,
            self.registry.generation(),
            self.session_id.clone(),
            turn_id.map(ToOwned::to_owned),
            payload,
        )
        .unwrap_or_else(|_| HookEventEnvelope {
            schema_version: 1,
            event,
            generation: self.registry.generation(),
            session_id: self.session_id.clone(),
            turn_id: turn_id.map(ToOwned::to_owned),
            payload: Value::Null,
        })
    }

    fn execution(
        &self,
        cancellation: CancellationToken,
    ) -> Option<(DefaultHookExecutor, HookRunContext<'_>)> {
        let executor = DefaultHookExecutor::new().ok()?;
        let context = HookRunContext {
            session_id: &self.session_id,
            workspace_root: &self.workspace_root,
            cancellation,
        };
        Some((executor, context))
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }
}
