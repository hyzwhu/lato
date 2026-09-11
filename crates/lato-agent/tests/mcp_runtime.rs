//! Phase 6C Task 5 — Unified Tool Safety Membrane for MCP.
//!
//! Critical security invariant: MCP tools are a ToolRegistry provider, NOT a
//! second execution channel. Every model-visible MCP call must traverse
//! PreToolUse → prepare_scoped → PolicyEngine → approval → execute → PostToolUse.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use lato_agent::{
    PromptKind, SessionActor, SessionHookRuntime, SessionMcpHandle, SkillRuntimeBinding,
    ToolApproval,
};
use lato_ai::{FakeModelStream, StreamPiece};
use lato_core::{
    ApprovalRequest, PolicyMode, SandboxProfile, SessionId, ToolCallId, ToolContext, ToolError,
    TurnId,
};
use lato_extensions::hooks::{HandlerType, HookEventName, HookRegistry, HookSpec};
use lato_mcp::{
    McpDescriptorSet, McpManager, McpSearchHit, McpToolDescriptor, qualify_tool,
};
use lato_policy::{ApprovalLedger, PolicyEngine};
use lato_tools::{
    BuiltinToolEnvironment, McpProviderConfig, McpToolBackend, PolicyScope, ToolRuntimeBuilder,
    bound_tool_output,
};
use lato_workspace::{FileLocks, SessionTrust};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct RecordingBackend {
    hits: Mutex<Vec<McpSearchHit>>,
    calls: AtomicUsize,
    last_call: Mutex<Option<(String, String, Value)>>,
    call_without_grant: AtomicUsize,
}

impl RecordingBackend {
    fn with_hits(hits: Vec<McpSearchHit>) -> Self {
        Self {
            hits: Mutex::new(hits),
            ..Self::default()
        }
    }
}

#[async_trait]
impl McpToolBackend for RecordingBackend {
    async fn ensure_index(&self) -> Result<(), ToolError> {
        Ok(())
    }

    fn search(&self, query: &str) -> Vec<McpSearchHit> {
        let needle = query.to_ascii_lowercase();
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|hit| hit.qualified_name.to_ascii_lowercase().contains(&needle))
            .cloned()
            .collect()
    }

    fn lookup(&self, qualified_or_parts: &str) -> Option<McpToolDescriptor> {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .find(|hit| hit.qualified_name == qualified_or_parts)
            .map(|hit| McpToolDescriptor {
                server: hit.server.clone(),
                name: hit.name.clone(),
                qualified_name: hit.qualified_name.clone(),
                description: hit.description.clone(),
                input_schema: hit.input_schema.clone(),
            })
    }

    fn tools_for_servers(&self, servers: &[String]) -> Vec<McpToolDescriptor> {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|hit| servers.iter().any(|s| s == &hit.server))
            .map(|hit| McpToolDescriptor {
                server: hit.server.clone(),
                name: hit.name.clone(),
                qualified_name: hit.qualified_name.clone(),
                description: hit.description.clone(),
                input_schema: hit.input_schema.clone(),
            })
            .collect()
    }

    async fn call_tool(
        &self,
        context: &ToolContext,
        server: &str,
        name: &str,
        arguments: Value,
    ) -> Result<Value, ToolError> {
        if context.execution_grant.is_none() {
            self.call_without_grant.fetch_add(1, Ordering::AcqRel);
            return Err(ToolError::new(
                "policy.grant_missing",
                "missing grant",
                lato_core::Retryability::Never,
            ));
        }
        self.calls.fetch_add(1, Ordering::AcqRel);
        *self.last_call.lock().unwrap() = Some((server.to_owned(), name.to_owned(), arguments));
        Ok(json!({
            "content": [{"type": "text", "text": "mcp-raw-output"}],
            "isError": false
        }))
    }
}

fn sample_hit() -> McpSearchHit {
    McpSearchHit {
        server: "demo".into(),
        name: "ping".into(),
        qualified_name: "demo__ping".into(),
        description: "Ping fixture".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "n": { "type": "integer" }
            },
            "additionalProperties": false
        }),
    }
}

fn context(call_id: &str) -> ToolContext {
    ToolContext {
        session_id: SessionId::from("mcp-membrane"),
        turn_id: TurnId::from("turn-1"),
        call_id: ToolCallId::from(call_id),
        cancellation: CancellationToken::new(),
        execution_grant: None,
    }
}

fn runtime_with_backend(
    root: &Path,
    backend: Arc<dyn McpToolBackend>,
    trusted: bool,
    mode: PolicyMode,
) -> Arc<lato_tools::ToolRuntime> {
    let trust = if trusted {
        SessionTrust::for_headless_prompt(root)
    } else {
        SessionTrust::for_interactive(root, false)
    };
    let scope = PolicyScope {
        workspace_root: root.to_path_buf(),
        mode,
        project_trusted: trust.cwd_trusted(),
        sandbox_profile: SandboxProfile::Off,
    };
    let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
        Duration::from_secs(60),
    ))));
    let mut builder = ToolRuntimeBuilder::new(policy, scope);
    builder
        .register_builtin_tools(BuiltinToolEnvironment {
            cwd: root.to_path_buf(),
            locks: Arc::new(FileLocks::new()),
            trust,
            skill_resolver: None,
        })
        .unwrap();
    builder
        .register_mcp_provider(backend, &McpProviderConfig::default())
        .unwrap();
    Arc::new(builder.build().unwrap())
}

struct AllowAll;
#[async_trait]
impl ToolApproval for AllowAll {
    async fn approve(&self, _request: &ApprovalRequest) -> bool {
        true
    }
}

struct DenyAll;
#[async_trait]
impl ToolApproval for DenyAll {
    async fn approve(&self, _request: &ApprovalRequest) -> bool {
        false
    }
}

fn write_hook_script(dir: &Path, name: &str, json_stdout: &str) -> PathBuf {
    let path = dir.join(name);
    // Emit JSON via a here-doc so nested quotes/braces stay intact.
    // Drain stdin first so the hook runner's payload write cannot race with a
    // short-lived process exit (EPIPE → fail-open Allow).
    let body = format!(
        "#!/bin/sh\ncat >/dev/null\ncat <<'LATO_HOOK_EOF'\n{json_stdout}\nLATO_HOOK_EOF\n"
    );
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    path
}

fn hook_spec(dir: &Path, id: &str, event: HookEventName, json_stdout: &str) -> HookSpec {
    let script = write_hook_script(dir, &format!("{id}.sh"), json_stdout);
    HookSpec {
        id: id.into(),
        plugin_name: "mcp-test".into(),
        event,
        handler_type: HandlerType::Command,
        matcher: None,
        command: Some(script.file_name().unwrap().to_string_lossy().into_owned()),
        url: None,
        timeout_ms: 2_000,
        source_dir: dir.to_path_buf(),
        extra_env: BTreeMap::new(),
    }
}

#[tokio::test]
async fn use_tool_requires_prepare_scoped_grant_before_backend_call() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(RecordingBackend::with_hits(vec![sample_hit()]));
    let runtime = runtime_with_backend(root.path(), backend.clone(), true, PolicyMode::Always);

    // Direct Tool::invoke without grant must not reach transport.
    let tools = lato_tools::mcp_provider_tools(backend.clone(), &McpProviderConfig::default());
    let use_tool = tools
        .into_iter()
        .find(|t| t.descriptor().name.local_name() == "use_tool")
        .unwrap();
    let err = use_tool
        .invoke(
            context("no-grant"),
            json!({"tool": "demo__ping", "arguments": {"n": 1}}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "policy.grant_missing");
    assert_eq!(backend.calls.load(Ordering::Acquire), 0);

    // Through ToolRuntime membrane — grant attached — call succeeds.
    let out = runtime
        .invoke(
            context("via-runtime"),
            "use_tool",
            json!({"tool": "demo__ping", "arguments": {"n": 1}}),
        )
        .await
        .unwrap();
    assert_eq!(out.metadata["kind"], "mcp_tool_result");
    assert_eq!(backend.calls.load(Ordering::Acquire), 1);
    assert_eq!(backend.call_without_grant.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn untrusted_policy_denies_before_mcp_execute() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(RecordingBackend::with_hits(vec![sample_hit()]));
    let runtime = runtime_with_backend(root.path(), backend.clone(), false, PolicyMode::Always);
    let err = runtime
        .invoke(
            context("denied"),
            "use_tool",
            json!({"tool": "demo__ping", "arguments": {"n": 1}}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "policy.untrusted_extension");
    assert_eq!(backend.calls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn ask_mode_requires_approval_before_mcp_execute() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(RecordingBackend::with_hits(vec![sample_hit()]));
    let runtime = runtime_with_backend(root.path(), backend.clone(), true, PolicyMode::Ask);

    let prepared = runtime
        .prepare_scoped(
            context("ask"),
            "use_tool",
            json!({"tool": "demo__ping", "arguments": {"n": 1}}),
            None,
        )
        .unwrap();
    assert!(matches!(
        runtime.decision(&prepared),
        lato_core::PolicyDecision::RequireApproval(_)
    ));
    assert_eq!(backend.calls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn session_mcp_handle_refuses_call_without_grant() {
    let handle = SessionMcpHandle::empty();
    handle
        .install(Arc::new(McpManager::new(
            McpDescriptorSet::empty(3),
            CancellationToken::new(),
        )))
        .await;
    assert_eq!(handle.snapshot_generation().await, 3);
    let err = McpToolBackend::call_tool(
        &handle,
        &context("direct"),
        "demo",
        "ping",
        json!({}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "policy.grant_missing");
}

#[tokio::test]
async fn skill_runtime_binding_registers_search_and_use_through_same_membrane() {
    let root = tempfile::tempdir().unwrap();
    let binding = SkillRuntimeBinding::builtin(
        root.path().to_path_buf(),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(root.path()),
    )
    .unwrap();
    let (runtime, _skill, mcp) = binding.into_parts();
    let names: Vec<_> = runtime
        .model_definitions()
        .into_iter()
        .filter_map(|d| {
            d.pointer("/function/name")
                .and_then(|n| n.as_str())
                .map(str::to_owned)
        })
        .collect();
    assert!(names.contains(&"search_tool".to_owned()));
    assert!(names.contains(&"use_tool".to_owned()));
    assert!(
        !names.iter().any(|n| n.contains("__")),
        "direct MCP expand must stay opt-in: {names:?}"
    );

    // Bind a recording-backed manager via empty install then ensure grant path.
    mcp.install_empty(9, CancellationToken::new()).await;
    assert_eq!(mcp.snapshot_generation().await, 9);

    let err = runtime
        .invoke(
            context("unbound-tool"),
            "use_tool",
            json!({"tool": "missing__tool", "arguments": {}}),
        )
        .await
        .unwrap_err();
    // Either tool not found after lookup, or manager transport error — but never
    // a bypass that returns a successful model result without the membrane.
    assert!(
        err.code.starts_with("mcp.")
            || err.code == "tool.invalid_arguments"
            || err.code == "mcp.tool_not_found"
            || err.code == "mcp.not_running"
            || err.code == "mcp.manager_unbound"
            || err.code == "mcp.execution_failed",
        "unexpected code {}",
        err.code
    );
}

#[tokio::test]
async fn actor_membrane_ordering_pre_prepare_policy_execute_post() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(RecordingBackend::with_hits(vec![sample_hit()]));
    let runtime = runtime_with_backend(root.path(), backend.clone(), true, PolicyMode::Always);

    let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut actor = SessionActor::new_with_tool_runtime(
        Arc::new(FakeModelStream::new(vec![
            vec![StreamPiece::ToolCall {
                id: "mcp-1".into(),
                name: "use_tool".into(),
                arguments: json!({"tool": "demo__ping", "arguments": {"n": 1}}),
            }],
            vec![StreamPiece::Text("done".into())],
        ])),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(root.path()),
        root.path().to_path_buf(),
        runtime,
    )
    .with_interactive_events(events, "mcp-session".into(), Some(Arc::new(AllowAll)));

    // PreToolUse allow (advisory) + PostToolUse updatedMCPToolOutput replacement.
    let hooks_dir = root.path().join("hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    let registry = HookRegistry::from_specs(
        11,
        vec![
            hook_spec(
                &hooks_dir,
                "pre-allow",
                HookEventName::PreToolUse,
                r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#,
            ),
            hook_spec(
                &hooks_dir,
                "post-mcp",
                HookEventName::PostToolUse,
                r#"{"hookSpecificOutput":{"updatedMCPToolOutput":{"text":"bounded-mcp-replacement"}}}"#,
            ),
        ],
    );
    actor.bind_turn_hooks(SessionHookRuntime::new(
        registry,
        root.path().to_path_buf(),
        "mcp-session".into(),
    ));

    actor
        .prompt(PromptKind::Start, "call mcp".into())
        .await
        .unwrap();

    assert_eq!(backend.calls.load(Ordering::Acquire), 1);
    let history = actor.history();
    let result = history.iter().find_map(|item| match item {
        lato_agent::HistoryItem::ToolResult { output, .. } => Some(output.as_str()),
        _ => None,
    });
    assert_eq!(result, Some("bounded-mcp-replacement"));
}

#[tokio::test]
async fn pre_tool_deny_blocks_mcp_before_execute() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(RecordingBackend::with_hits(vec![sample_hit()]));
    let runtime = runtime_with_backend(root.path(), backend.clone(), true, PolicyMode::Always);
    let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut actor = SessionActor::new_with_tool_runtime(
        Arc::new(FakeModelStream::new(vec![
            vec![StreamPiece::ToolCall {
                id: "mcp-deny".into(),
                name: "use_tool".into(),
                arguments: json!({"tool": "demo__ping", "arguments": {"n": 1}}),
            }],
            vec![StreamPiece::Text("done".into())],
        ])),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(root.path()),
        root.path().to_path_buf(),
        runtime,
    )
    .with_interactive_events(events, "mcp-deny".into(), Some(Arc::new(AllowAll)));

    let hooks_dir = root.path().join("hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    actor.bind_turn_hooks(SessionHookRuntime::new(
        HookRegistry::from_specs(
            12,
            vec![hook_spec(
                &hooks_dir,
                "deny",
                HookEventName::PreToolUse,
                r#"{"decision":"block","reason":"no mcp"}"#,
            )],
        ),
        root.path().to_path_buf(),
        "mcp-deny".into(),
    ));

    actor.prompt(PromptKind::Start, "nope".into()).await.unwrap();
    assert_eq!(backend.calls.load(Ordering::Acquire), 0);
    assert!(actor.history().iter().any(|item| matches!(
        item,
        lato_agent::HistoryItem::ToolResult { output, .. }
            if output.contains("hook.pre_tool_denied")
    )));
}

#[tokio::test]
async fn pre_tool_ask_still_requires_approval_and_hook_allow_never_skips_policy() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(RecordingBackend::with_hits(vec![sample_hit()]));
    // Untrusted project: policy must deny even if PreToolUse says allow.
    let runtime = runtime_with_backend(root.path(), backend.clone(), false, PolicyMode::Always);
    let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut actor = SessionActor::new_with_tool_runtime(
        Arc::new(FakeModelStream::new(vec![
            vec![StreamPiece::ToolCall {
                id: "mcp-allow-untrusted".into(),
                name: "use_tool".into(),
                arguments: json!({"tool": "demo__ping", "arguments": {"n": 1}}),
            }],
            vec![StreamPiece::Text("done".into())],
        ])),
        Arc::new(FileLocks::new()),
        SessionTrust::for_interactive(root.path(), false),
        root.path().to_path_buf(),
        runtime,
    )
    .with_interactive_events(events, "mcp-untrusted".into(), Some(Arc::new(AllowAll)));

    let hooks_dir = root.path().join("hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    actor.bind_turn_hooks(SessionHookRuntime::new(
        HookRegistry::from_specs(
            13,
            vec![hook_spec(
                &hooks_dir,
                "allow",
                HookEventName::PreToolUse,
                r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#,
            )],
        ),
        root.path().to_path_buf(),
        "mcp-untrusted".into(),
    ));

    actor
        .prompt(PromptKind::Start, "try".into())
        .await
        .unwrap();
    assert_eq!(backend.calls.load(Ordering::Acquire), 0);
    assert!(actor.history().iter().any(|item| matches!(
        item,
        lato_agent::HistoryItem::ToolResult { output, .. }
            if output.contains("policy.untrusted_extension")
    )));
}

#[tokio::test]
async fn pre_tool_rewrite_reenters_prepare_scoped_with_immutable_tool_name() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(RecordingBackend::with_hits(vec![sample_hit()]));
    let runtime = runtime_with_backend(root.path(), backend.clone(), true, PolicyMode::Always);
    let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut actor = SessionActor::new_with_tool_runtime(
        Arc::new(FakeModelStream::new(vec![
            vec![StreamPiece::ToolCall {
                id: "mcp-rewrite".into(),
                name: "use_tool".into(),
                arguments: json!({"tool": "demo__ping", "arguments": {"n": 1}}),
            }],
            vec![StreamPiece::Text("done".into())],
        ])),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(root.path()),
        root.path().to_path_buf(),
        runtime,
    )
    .with_interactive_events(events, "mcp-rewrite".into(), Some(Arc::new(AllowAll)));

    // Rewrite arguments only — tool name stays use_tool; prepare_scoped re-validates.
    let hooks_dir = root.path().join("hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    actor.bind_turn_hooks(SessionHookRuntime::new(
        HookRegistry::from_specs(
            14,
            vec![hook_spec(
                &hooks_dir,
                "rewrite",
                HookEventName::PreToolUse,
                r#"{"hookSpecificOutput":{"updatedInput":{"tool":"demo__ping","arguments":{"n":2}}}}"#,
            )],
        ),
        root.path().to_path_buf(),
        "mcp-rewrite".into(),
    ));

    actor
        .prompt(PromptKind::Start, "rewrite".into())
        .await
        .unwrap();
    assert_eq!(backend.calls.load(Ordering::Acquire), 1);
    let last = backend.last_call.lock().unwrap().clone().unwrap();
    assert_eq!(last.0, "demo");
    assert_eq!(last.1, "ping");
    assert_eq!(last.2, json!({"n": 2}));
}

#[tokio::test]
async fn pre_tool_ask_routes_through_approval_before_execute() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(RecordingBackend::with_hits(vec![sample_hit()]));
    let runtime = runtime_with_backend(root.path(), backend.clone(), true, PolicyMode::Always);
    let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut actor = SessionActor::new_with_tool_runtime(
        Arc::new(FakeModelStream::new(vec![
            vec![StreamPiece::ToolCall {
                id: "mcp-ask".into(),
                name: "use_tool".into(),
                arguments: json!({"tool": "demo__ping", "arguments": {"n": 1}}),
            }],
            vec![StreamPiece::Text("done".into())],
        ])),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(root.path()),
        root.path().to_path_buf(),
        runtime,
    )
    .with_interactive_events(events, "mcp-ask".into(), Some(Arc::new(DenyAll)));

    let hooks_dir = root.path().join("hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    actor.bind_turn_hooks(SessionHookRuntime::new(
        HookRegistry::from_specs(
            15,
            vec![hook_spec(
                &hooks_dir,
                "ask",
                HookEventName::PreToolUse,
                r#"{"decision":"ask","reason":"confirm mcp"}"#,
            )],
        ),
        root.path().to_path_buf(),
        "mcp-ask".into(),
    ));

    actor.prompt(PromptKind::Start, "ask".into()).await.unwrap();
    assert_eq!(backend.calls.load(Ordering::Acquire), 0);
    assert!(actor.history().iter().any(|item| matches!(
        item,
        lato_agent::HistoryItem::ToolResult { output, .. }
            if output.contains("policy.approval_denied")
    )));
}

#[tokio::test]
async fn updated_mcp_tool_output_is_rebound_via_bound_tool_output() {
    let root = tempfile::tempdir().unwrap();
    let huge = "X".repeat(30_000);
    let payload = format!(
        "{{\"hookSpecificOutput\":{{\"updatedMCPToolOutput\":{{\"text\":\"{huge}\"}}}}}}"
    );
    let hooks = SessionHookRuntime::new(
        HookRegistry::from_specs(
            16,
            vec![hook_spec(root.path(), "post", HookEventName::PostToolUse, &payload)],
        ),
        root.path().to_path_buf(),
        "session".into(),
    );
    let post = hooks
        .post_tool_use(
            "turn",
            json!({
                "toolName": "use_tool",
                "arguments": {},
                "success": true,
                "output": "mcp-raw-output",
            }),
            CancellationToken::new(),
        )
        .await;
    let replacement = post.replacement.expect("mcp replacement");
    let text = replacement["text"].as_str().unwrap().to_owned();
    let bounded = bound_tool_output(text, root.path(), "call-mcp").await.unwrap();
    assert!(
        bounded.len() < 30_000 || bounded.contains("tool-output"),
        "MCP replacement must pass bound_tool_output"
    );
}

#[test]
fn qualify_tool_wire_name_is_stable() {
    assert_eq!(qualify_tool("demo", "ping"), "demo__ping");
}

#[tokio::test]
async fn no_public_agent_bypass_of_mcp_manager_call_tool() {
    // Behavioral stand-in for "no agent path calls McpManager::call_tool without
    // ToolRuntime": SessionMcpHandle (the only agent-facing backend) refuses
    // ungated calls, and SkillRuntimeBinding exposes MCP only via catalog tools.
    let handle = SessionMcpHandle::default();
    let err = McpToolBackend::call_tool(&handle, &context("x"), "s", "t", json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "policy.grant_missing");
}

