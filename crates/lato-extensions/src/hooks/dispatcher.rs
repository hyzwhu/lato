use std::{sync::Arc, time::Instant};

use async_trait::async_trait;
use serde_json::Value;

use super::{
    HandlerType, HookEventEnvelope, HookEventName, HookRegistry, HookRunContext, HookRunError,
    HookSpec, MAX_CONTEXT_BYTES, ParsedDecision, ParsedHookResult, RawHookRun,
    SystemHookDnsResolver, build_hook_http_client, parse_hook_result, run_command_hook,
    run_http_hook,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookDecision {
    Allow,
    Ask {
        hook_id: String,
        reason: Option<String>,
    },
    Defer {
        hook_id: String,
    },
    Deny {
        hook_id: String,
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookContext {
    pub hook_id: String,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookBlock {
    pub hook_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookRunOutcome {
    Skipped,
    Completed,
    Failed,
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookRunRecord {
    pub hook_id: String,
    pub outcome: HookRunOutcome,
    pub duration_ms: u64,
    pub feedback: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreToolUseResult {
    pub decision: HookDecision,
    pub updated_input: Option<Value>,
    pub additional_context: Vec<HookContext>,
    pub runs: Vec<HookRunRecord>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PostToolUseResult {
    pub blocks: Vec<HookBlock>,
    pub additional_context: Vec<HookContext>,
    pub replacement: Option<Value>,
    pub runs: Vec<HookRunRecord>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StopResult {
    pub blocks: Vec<HookBlock>,
    pub additional_context: Vec<HookContext>,
    pub prevent_continuation: Option<HookBlock>,
    pub runs: Vec<HookRunRecord>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PromptResult {
    pub block: Option<HookBlock>,
    pub runs: Vec<HookRunRecord>,
}

#[async_trait]
pub trait HookExecutor: Send + Sync {
    async fn execute(
        &self,
        spec: &HookSpec,
        envelope: &HookEventEnvelope,
        context: &HookRunContext<'_>,
    ) -> Result<RawHookRun, HookRunError>;
}

pub struct DefaultHookExecutor {
    client: reqwest::Client,
    resolver: Arc<dyn super::HookDnsResolver>,
}

impl DefaultHookExecutor {
    pub fn new() -> Result<Self, HookRunError> {
        Ok(Self {
            client: build_hook_http_client()?,
            resolver: Arc::new(SystemHookDnsResolver),
        })
    }
}

#[async_trait]
impl HookExecutor for DefaultHookExecutor {
    async fn execute(
        &self,
        spec: &HookSpec,
        envelope: &HookEventEnvelope,
        context: &HookRunContext<'_>,
    ) -> Result<RawHookRun, HookRunError> {
        match spec.handler_type {
            HandlerType::Command => run_command_hook(spec, envelope, context).await,
            HandlerType::Http => {
                run_http_hook(
                    spec,
                    envelope,
                    context,
                    &self.client,
                    self.resolver.as_ref(),
                )
                .await
            }
        }
    }
}

pub async fn dispatch_pre_tool_use(
    registry: &HookRegistry,
    envelope: &HookEventEnvelope,
    context: &HookRunContext<'_>,
    executor: &dyn HookExecutor,
) -> PreToolUseResult {
    let mut current = envelope
        .payload
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Null);
    let original = current.clone();
    let mut decision = HookDecision::Allow;
    let mut contexts = Vec::new();
    let mut context_bytes = 0;
    let mut runs = Vec::new();
    for spec in registry.handlers(HookEventName::PreToolUse) {
        if !eligible(spec, envelope) {
            runs.push(skipped(spec));
            continue;
        }
        let mut next = envelope.clone();
        if let Some(object) = next.payload.as_object_mut() {
            object.insert("arguments".into(), current.clone());
        }
        let Some(parsed) = execute_parsed(spec, &next, context, executor, &mut runs).await else {
            continue;
        };
        if let Some(updated) = parsed.updated_input {
            current = updated;
        }
        if let Some(text) = parsed.additional_context {
            push_context(&mut contexts, &mut context_bytes, &spec.id, text);
        }
        match parsed.decision {
            ParsedDecision::Deny => {
                return PreToolUseResult {
                    decision: HookDecision::Deny {
                        hook_id: spec.id.clone(),
                        reason: parsed.reason.unwrap_or_else(|| "blocked by hook".into()),
                    },
                    updated_input: None,
                    additional_context: Vec::new(),
                    runs,
                };
            }
            ParsedDecision::Ask => {
                decision = HookDecision::Ask {
                    hook_id: spec.id.clone(),
                    reason: parsed.reason,
                }
            }
            ParsedDecision::Defer if matches!(decision, HookDecision::Allow) => {
                decision = HookDecision::Defer {
                    hook_id: spec.id.clone(),
                }
            }
            _ => {}
        }
    }
    PreToolUseResult {
        decision,
        updated_input: (current != original).then_some(current),
        additional_context: contexts,
        runs,
    }
}

pub async fn dispatch_prompt_submit(
    registry: &HookRegistry,
    envelope: &HookEventEnvelope,
    context: &HookRunContext<'_>,
    executor: &dyn HookExecutor,
) -> PromptResult {
    let mut runs = Vec::new();
    for spec in registry.handlers(HookEventName::UserPromptSubmit) {
        if let Some(parsed) = execute_parsed(spec, envelope, context, executor, &mut runs).await
            && parsed.decision == ParsedDecision::Deny
        {
            return PromptResult {
                block: Some(HookBlock {
                    hook_id: spec.id.clone(),
                    reason: parsed.reason.unwrap_or_else(|| "blocked by hook".into()),
                }),
                runs,
            };
        }
    }
    PromptResult { block: None, runs }
}

pub async fn dispatch_post_tool_use(
    registry: &HookRegistry,
    envelope: &HookEventEnvelope,
    context: &HookRunContext<'_>,
    executor: &dyn HookExecutor,
) -> PostToolUseResult {
    let mut result = PostToolUseResult {
        blocks: Vec::new(),
        additional_context: Vec::new(),
        replacement: None,
        runs: Vec::new(),
    };
    let mut context_bytes = 0;
    for spec in registry.handlers(HookEventName::PostToolUse) {
        if !eligible(spec, envelope) {
            result.runs.push(skipped(spec));
            continue;
        }
        let Some(parsed) =
            execute_parsed(spec, envelope, context, executor, &mut result.runs).await
        else {
            continue;
        };
        if parsed.decision == ParsedDecision::Deny {
            result.blocks.push(HookBlock {
                hook_id: spec.id.clone(),
                reason: parsed
                    .reason
                    .clone()
                    .unwrap_or_else(|| "blocked by hook".into()),
            });
        }
        if let Some(text) = parsed.additional_context {
            push_context(
                &mut result.additional_context,
                &mut context_bytes,
                &spec.id,
                text,
            );
        }
        // Phase 6C: updatedMCPToolOutput is now applied (MCP result type exists).
        // Prefer the MCP-specific field when a handler returns both.
        if let Some(replacement) = parsed
            .updated_mcp_tool_output
            .or(parsed.updated_tool_output)
        {
            result.replacement = Some(replacement);
        }
    }
    result
}

pub async fn dispatch_stop(
    registry: &HookRegistry,
    envelope: &HookEventEnvelope,
    context: &HookRunContext<'_>,
    executor: &dyn HookExecutor,
) -> StopResult {
    let mut result = StopResult {
        blocks: Vec::new(),
        additional_context: Vec::new(),
        prevent_continuation: None,
        runs: Vec::new(),
    };
    let mut context_bytes = 0;
    for spec in registry.handlers(HookEventName::Stop) {
        let Some(parsed) =
            execute_parsed(spec, envelope, context, executor, &mut result.runs).await
        else {
            continue;
        };
        if parsed.decision == ParsedDecision::Deny {
            result.blocks.push(HookBlock {
                hook_id: spec.id.clone(),
                reason: parsed
                    .reason
                    .clone()
                    .unwrap_or_else(|| "blocked by hook".into()),
            });
        }
        if let Some(text) = parsed.additional_context {
            push_context(
                &mut result.additional_context,
                &mut context_bytes,
                &spec.id,
                text,
            );
        }
        if parsed.continue_ == Some(false) {
            result.prevent_continuation = Some(HookBlock {
                hook_id: spec.id.clone(),
                reason: parsed
                    .stop_reason
                    .or(parsed.reason)
                    .unwrap_or_else(|| "continuation prevented by hook".into()),
            });
        }
    }
    result
}

pub async fn dispatch_observer(
    registry: &HookRegistry,
    event: HookEventName,
    envelope: &HookEventEnvelope,
    context: &HookRunContext<'_>,
    executor: &dyn HookExecutor,
) -> Vec<HookRunRecord> {
    debug_assert_eq!(event.mode(), super::HookMode::Observe);
    let mut runs = Vec::new();
    for spec in registry.handlers(event) {
        if !eligible(spec, envelope) {
            runs.push(skipped(spec));
            continue;
        }
        let _ = execute_parsed(spec, envelope, context, executor, &mut runs).await;
    }
    runs
}

async fn execute_parsed(
    spec: &HookSpec,
    envelope: &HookEventEnvelope,
    context: &HookRunContext<'_>,
    executor: &dyn HookExecutor,
    runs: &mut Vec<HookRunRecord>,
) -> Option<ParsedHookResult> {
    let started = Instant::now();
    match executor.execute(spec, envelope, context).await {
        Ok(raw) if matches!(raw.exit_code, Some(0 | 2)) => {
            match parse_hook_result(spec.event, &raw.stdout, raw.exit_code) {
                Ok(parsed) => {
                    runs.push(HookRunRecord {
                        hook_id: spec.id.clone(),
                        outcome: HookRunOutcome::Completed,
                        duration_ms: millis(raw.elapsed),
                        feedback: parsed.system_message.clone(),
                    });
                    Some(parsed)
                }
                Err(_) => {
                    runs.push(failed(spec, HookRunOutcome::Failed, started));
                    None
                }
            }
        }
        Ok(_) => {
            runs.push(failed(spec, HookRunOutcome::Failed, started));
            None
        }
        Err(HookRunError::Timeout { .. }) => {
            runs.push(failed(spec, HookRunOutcome::TimedOut, started));
            None
        }
        Err(HookRunError::Cancelled) => {
            runs.push(failed(spec, HookRunOutcome::Cancelled, started));
            None
        }
        Err(_) => {
            runs.push(failed(spec, HookRunOutcome::Failed, started));
            None
        }
    }
}

fn eligible(spec: &HookSpec, envelope: &HookEventEnvelope) -> bool {
    if matches!(
        spec.event,
        HookEventName::UserPromptSubmit | HookEventName::Stop
    ) {
        return true;
    }
    let match_value = envelope
        .payload
        .get("toolName")
        .and_then(Value::as_str)
        .or_else(|| envelope.payload.get("matcher").and_then(Value::as_str))
        .unwrap_or("");
    spec.matcher
        .as_ref()
        .is_none_or(|matcher| matcher.matches(match_value))
}

fn push_context(contexts: &mut Vec<HookContext>, bytes: &mut usize, hook_id: &str, text: String) {
    if bytes.saturating_add(text.len()) <= MAX_CONTEXT_BYTES {
        *bytes += text.len();
        contexts.push(HookContext {
            hook_id: hook_id.into(),
            text,
        });
    }
}

fn skipped(spec: &HookSpec) -> HookRunRecord {
    HookRunRecord {
        hook_id: spec.id.clone(),
        outcome: HookRunOutcome::Skipped,
        duration_ms: 0,
        feedback: None,
    }
}
fn failed(spec: &HookSpec, outcome: HookRunOutcome, started: Instant) -> HookRunRecord {
    HookRunRecord {
        hook_id: spec.id.clone(),
        outcome,
        duration_ms: millis(started.elapsed()),
        feedback: None,
    }
}
fn millis(duration: std::time::Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}
