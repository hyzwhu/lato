//! Phase 6C Task 5 — Unified Tool Safety Membrane for MCP.
//!
//! Critical security invariant: MCP tools are a ToolRegistry provider, NOT a
//! second execution channel. Every model-visible MCP call must traverse
//! PreToolUse → prepare_scoped → PolicyEngine → approval → execute → PostToolUse.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use lato_agent::{
    PromptKind, RuntimePromptOutcome, RuntimeSession, SessionActor, SessionHookRuntime,
    SessionMcpHandle, SessionPluginSnapshots, SkillRuntimeBinding, ToolApproval,
};
use lato_ai::{FakeModelStream, StreamPiece};
use lato_core::{
    ApprovalRequest, PolicyMode, SandboxProfile, SessionId, ToolCallId, ToolContext, ToolError,
    TurnId,
};
use lato_extensions::hooks::{HandlerType, HookEventName, HookRegistry, HookSpec};
use lato_extensions::{
    CapabilityCeiling, DiscoveryConfig, McpCapabilityCeiling, PluginConfig, PluginSnapshot,
    build_snapshot, discover_plugins, materialize_mcp,
};
use lato_mcp::{McpDescriptorSet, McpManager, McpSearchHit, McpToolDescriptor, qualify_tool};
use lato_policy::{ApprovalLedger, PolicyEngine};
use lato_tools::{
    BuiltinToolEnvironment, McpProviderConfig, McpToolBackend, PolicyScope, ToolRuntimeBuilder,
    bound_tool_output,
};
use lato_workspace::{FileLocks, SessionTrust};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};
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
    // Emit JSON via a here-doc so nested quotes/braces stay intact.
    // Drain stdin first so the hook runner's payload write cannot race with a
    // short-lived process exit (EPIPE → fail-open Allow).
    if cfg!(windows) {
        // Windows cannot execute shebang scripts; use a batch file instead.
        // The echo appends CRLF, which the JSON parser tolerates.
        let path = dir.join(format!("{name}.cmd"));
        std::fs::write(
            &path,
            format!("@echo off\r\nmore > NUL\r\necho {json_stdout}\r\n"),
        )
        .unwrap();
        return path;
    }
    let path = dir.join(name);
    let body =
        format!("#!/bin/sh\ncat >/dev/null\ncat <<'LATO_HOOK_EOF'\n{json_stdout}\nLATO_HOOK_EOF\n");
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
    let err = McpToolBackend::call_tool(&handle, &context("direct"), "demo", "ping", json!({}))
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

    actor
        .prompt(PromptKind::Start, "nope".into())
        .await
        .unwrap();
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

    actor.prompt(PromptKind::Start, "try".into()).await.unwrap();
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
    let payload =
        format!("{{\"hookSpecificOutput\":{{\"updatedMCPToolOutput\":{{\"text\":\"{huge}\"}}}}}}");
    let hooks = SessionHookRuntime::new(
        HookRegistry::from_specs(
            16,
            vec![hook_spec(
                root.path(),
                "post",
                HookEventName::PostToolUse,
                &payload,
            )],
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
    let bounded = bound_tool_output(text, root.path(), "call-mcp")
        .await
        .unwrap();
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

// --- Phase 6C Task 7: reload & parent/child capability narrowing ---

struct GatedStream {
    started: Semaphore,
    release: Semaphore,
    contexts: Mutex<Vec<Value>>,
}

impl Default for GatedStream {
    fn default() -> Self {
        Self {
            started: Semaphore::new(0),
            release: Semaphore::new(0),
            contexts: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl lato_ai::ModelStream for GatedStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        context: Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), lato_core::ModelError> {
        self.contexts.lock().unwrap().push(context);
        self.started.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        tx.send(StreamPiece::Text("done".into()))
            .await
            .map_err(|_| lato_core::ModelError::cancelled())
    }
}

fn mcp_plugin_snapshot(generation: u64, servers: &[(&str, &str)]) -> Arc<PluginSnapshot> {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    let plugin = root.path().join("plugin");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&plugin).unwrap();
    let mut mcp_servers = serde_json::Map::new();
    for (name, command) in servers {
        mcp_servers.insert(
            (*name).into(),
            json!({"command": command, "args": ["server.js"]}),
        );
    }
    std::fs::write(
        plugin.join("plugin.json"),
        json!({
            "name": "demo",
            "mcpServers": mcp_servers,
        })
        .to_string(),
    )
    .unwrap();
    // Keep tempdir alive by leaking — tests are short-lived.
    std::mem::forget(root);
    build_snapshot(
        generation,
        discover_plugins(&DiscoveryConfig {
            cwd: workspace,
            lato_home: home,
            cli_plugin_dirs: vec![plugin],
            project_trusted: true,
        }),
        &PluginConfig::default(),
    )
    .unwrap()
}

#[tokio::test]
async fn mid_turn_reload_keeps_gen_n_mcp_and_next_turn_adopts_n_plus_one() {
    let old = mcp_plugin_snapshot(1, &[("alpha", "node")]);
    let new = mcp_plugin_snapshot(2, &[("beta", "node")]);
    assert_eq!(materialize_mcp(&old).servers[0].server_name, "alpha");
    assert_eq!(materialize_mcp(&new).servers[0].server_name, "beta");

    let stream = Arc::new(GatedStream::default());
    let directory = tempfile::tempdir().unwrap();
    let (updates, _rx) = mpsc::unbounded_channel();
    let session = Arc::new(RuntimeSession::new(
        "mcp-reload".into(),
        stream.clone(),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(directory.path()),
        directory.path().to_path_buf(),
        updates,
        None,
    ));
    session.stage_plugin_snapshot(old).await.unwrap();

    let first = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.prompt("first".into()).await }
    });
    stream.started.acquire().await.unwrap().forget();

    assert_eq!(
        session
            .active_turn_plugin_snapshot()
            .await
            .unwrap()
            .generation(),
        1
    );
    assert_eq!(session.mcp_generation().await, Some(1));
    let active_set = materialize_mcp(&session.active_turn_plugin_snapshot().await.unwrap());
    assert_eq!(active_set.servers[0].server_name, "alpha");

    session.stage_plugin_snapshot(new).await.unwrap();
    // Mid-turn: staged N+1 must not mutate the live MCP generation.
    assert_eq!(session.mcp_generation().await, Some(1));
    assert_eq!(
        session
            .active_turn_plugin_snapshot()
            .await
            .unwrap()
            .generation(),
        1
    );
    assert_eq!(session.plugin_snapshot().await.generation(), 1);

    stream.release.add_permits(1);
    assert!(matches!(
        first.await.unwrap().unwrap(),
        RuntimePromptOutcome::Complete { .. }
    ));
    assert_eq!(session.plugin_snapshot().await.generation(), 2);

    // After turn 1, gen 1 should already be retired once turn 2's begin adopts gen 2.
    // Probe retired generations only while idle (driver lock is free).
    let second = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.prompt("second".into()).await }
    });
    stream.started.acquire().await.unwrap().forget();
    assert_eq!(session.mcp_generation().await, Some(2));
    let active_set = materialize_mcp(&session.active_turn_plugin_snapshot().await.unwrap());
    assert_eq!(active_set.servers[0].server_name, "beta");
    stream.release.add_permits(1);
    second.await.unwrap().unwrap();

    let retired = session.mcp_retired_generations().await;
    assert!(
        retired.contains(&1),
        "generation 1 must be retired when 2 is adopted: {retired:?}"
    );
    assert_eq!(session.mcp_generation().await, Some(2));
}

#[tokio::test]
async fn adopt_generation_retires_previous_with_cancel_and_reap() {
    let handle = SessionMcpHandle::empty();
    let cancel_a = CancellationToken::new();
    let manager_a = Arc::new(McpManager::new(
        McpDescriptorSet::empty(10),
        cancel_a.clone(),
    ));
    handle.adopt_generation(manager_a).await;
    assert_eq!(handle.snapshot_generation().await, 10);

    let cancel_b = CancellationToken::new();
    let manager_b = Arc::new(McpManager::new(
        McpDescriptorSet::empty(11),
        cancel_b.clone(),
    ));
    handle.adopt_generation(manager_b).await;
    assert_eq!(handle.snapshot_generation().await, 11);
    assert!(cancel_a.is_cancelled(), "retired generation must cancel");
    assert!(
        handle.retired_generations().await.contains(&10),
        "retired generation recorded"
    );

    // Same-generation re-adopt is a no-op (keeps live manager, no extra retire).
    let before = handle.retired_generations().await.len();
    handle
        .adopt_generation(Arc::new(McpManager::new(
            McpDescriptorSet::empty(11),
            CancellationToken::new(),
        )))
        .await;
    assert_eq!(handle.retired_generations().await.len(), before);
    assert_eq!(handle.snapshot_generation().await, 11);
}

#[tokio::test]
async fn child_cannot_restore_removed_mcp_server_or_tool() {
    let parent = mcp_plugin_snapshot(5, &[("demo", "node"), ("other", "node")]);
    let parent_set = materialize_mcp(&parent);
    assert_eq!(parent_set.servers.len(), 2);

    let narrowed = parent.derive_child(&CapabilityCeiling {
        parent: vec![lato_core::ToolCapability::ExtensionInvoke],
        profile: vec![lato_core::ToolCapability::ExtensionInvoke],
        workspace: vec![lato_core::ToolCapability::ExtensionInvoke],
        mcp: McpCapabilityCeiling {
            allowed_servers: Some(BTreeSet::from(["demo".into()])),
            allowed_tools: Some(BTreeSet::from([qualify_tool("demo", "ping")])),
        },
        workflows: Default::default(),
    });
    assert_eq!(
        narrowed.mcp_ceiling().allowed_servers,
        Some(BTreeSet::from(["demo".into()]))
    );
    let child_set = materialize_mcp(&narrowed);
    assert_eq!(child_set.servers.len(), 1);
    assert_eq!(child_set.servers[0].server_name, "demo");
    assert!(child_set.allows_tool(&qualify_tool("demo", "ping")));
    assert!(!child_set.allows_tool(&qualify_tool("demo", "secret")));
    assert!(!child_set.allows_tool(&qualify_tool("other", "ping")));

    // Nested child attempts to restore `other` / extra tools — intersection blocks it.
    let restored = narrowed.derive_child(&CapabilityCeiling {
        parent: vec![lato_core::ToolCapability::ExtensionInvoke],
        profile: vec![lato_core::ToolCapability::ExtensionInvoke],
        workspace: vec![lato_core::ToolCapability::ExtensionInvoke],
        mcp: McpCapabilityCeiling {
            allowed_servers: Some(BTreeSet::from(["demo".into(), "other".into()])),
            allowed_tools: Some(BTreeSet::from([
                qualify_tool("demo", "ping"),
                qualify_tool("demo", "secret"),
                qualify_tool("other", "ping"),
            ])),
        },
        workflows: Default::default(),
    });
    let restored_set = materialize_mcp(&restored);
    assert_eq!(restored_set.servers.len(), 1);
    assert_eq!(restored_set.servers[0].server_name, "demo");
    assert!(restored_set.allows_tool(&qualify_tool("demo", "ping")));
    assert!(!restored_set.allows_tool(&qualify_tool("demo", "secret")));
    assert!(!restored_set.allows_tool(&qualify_tool("other", "ping")));
}

#[tokio::test]
async fn parent_reload_does_not_mutate_running_child_snapshot() {
    let parent_snap = mcp_plugin_snapshot(1, &[("demo", "node")]);
    let child_snap = parent_snap.derive_child(&CapabilityCeiling {
        parent: vec![lato_core::ToolCapability::ExtensionInvoke],
        profile: vec![lato_core::ToolCapability::ExtensionInvoke],
        workspace: vec![lato_core::ToolCapability::ExtensionInvoke],
        mcp: McpCapabilityCeiling {
            allowed_servers: Some(BTreeSet::from(["demo".into()])),
            allowed_tools: None,
        },
        workflows: Default::default(),
    });

    let table = SessionPluginSnapshots::default();
    table
        .register(SessionId::from("parent"), Arc::clone(&parent_snap))
        .await;
    table
        .register(SessionId::from("child"), Arc::clone(&child_snap))
        .await;

    let directory = tempfile::tempdir().unwrap();
    let (updates, _rx) = mpsc::unbounded_channel();
    let stream = Arc::new(GatedStream::default());
    let child_session = Arc::new(
        RuntimeSession::new_child(lato_agent::ChildSessionConfig {
            session_id: "child".into(),
            stream: stream.clone(),
            locks: Arc::new(FileLocks::new()),
            trust: SessionTrust::for_headless_prompt(directory.path()),
            cwd: directory.path().to_path_buf(),
            updates,
            approval: None,
            tool_runtime: lato_agent::ChildToolRuntime::from(
                SkillRuntimeBinding::builtin(
                    directory.path().to_path_buf(),
                    Arc::new(FileLocks::new()),
                    SessionTrust::for_headless_prompt(directory.path()),
                )
                .unwrap(),
            ),
            initial_history: vec![],
            plugin_snapshot: Arc::clone(&child_snap),
        })
        .await
        .unwrap(),
    );

    let prompt = tokio::spawn({
        let session = Arc::clone(&child_session);
        async move { session.prompt("child-turn".into()).await }
    });
    stream.started.acquire().await.unwrap().forget();

    let before = materialize_mcp(&child_session.active_turn_plugin_snapshot().await.unwrap());
    assert_eq!(before.servers.len(), 1);

    // Parent reloads to a generation that removes demo / adds other.
    let parent_reload = mcp_plugin_snapshot(2, &[("other", "node")]);
    table
        .adopt(SessionId::from("parent"), Arc::clone(&parent_reload))
        .await;
    assert_eq!(
        table
            .get(&SessionId::from("parent"))
            .await
            .unwrap()
            .generation(),
        2
    );
    // Child table entry and live child session remain frozen.
    assert_eq!(
        table
            .get(&SessionId::from("child"))
            .await
            .unwrap()
            .generation(),
        1
    );
    assert_eq!(
        child_session
            .active_turn_plugin_snapshot()
            .await
            .unwrap()
            .generation(),
        1
    );
    let after = materialize_mcp(&child_session.active_turn_plugin_snapshot().await.unwrap());
    assert_eq!(after.servers.len(), 1);
    assert_eq!(after.servers[0].server_name, "demo");

    stream.release.add_permits(1);
    prompt.await.unwrap().unwrap();
}

#[tokio::test]
async fn server_and_tool_narrowing_on_child_derivation_filters_manager() {
    let parent = mcp_plugin_snapshot(3, &[("keep", "node"), ("drop", "node")]);
    let child = parent.derive_child(&CapabilityCeiling {
        parent: vec![lato_core::ToolCapability::ExtensionInvoke],
        profile: vec![lato_core::ToolCapability::ExtensionInvoke],
        workspace: vec![lato_core::ToolCapability::ExtensionInvoke],
        mcp: McpCapabilityCeiling {
            allowed_servers: Some(BTreeSet::from(["keep".into()])),
            allowed_tools: Some(BTreeSet::from([qualify_tool("keep", "ok")])),
        },
        workflows: Default::default(),
    });
    let descriptors = materialize_mcp(&child);
    assert_eq!(descriptors.servers.len(), 1);
    assert_eq!(descriptors.servers[0].server_name, "keep");
    let manager = McpManager::new(descriptors, CancellationToken::new());
    let denied = manager
        .call_tool("keep", "nope", json!({}))
        .await
        .unwrap_err();
    assert_eq!(denied.code(), "mcp.capability_denied");
    // Dropped server is absent from descriptors; tool ceiling also denies it.
    let missing_server = manager
        .call_tool("drop", "ok", json!({}))
        .await
        .unwrap_err();
    assert_eq!(missing_server.code(), "mcp.capability_denied");

    let server_only = parent.derive_child(&CapabilityCeiling {
        parent: vec![lato_core::ToolCapability::ExtensionInvoke],
        profile: vec![lato_core::ToolCapability::ExtensionInvoke],
        workspace: vec![lato_core::ToolCapability::ExtensionInvoke],
        mcp: McpCapabilityCeiling {
            allowed_servers: Some(BTreeSet::from(["keep".into()])),
            allowed_tools: None,
        },
        workflows: Default::default(),
    });
    let manager = McpManager::new(materialize_mcp(&server_only), CancellationToken::new());
    let missing = manager
        .call_tool("drop", "anything", json!({}))
        .await
        .unwrap_err();
    assert_eq!(missing.code(), "mcp.not_running");
}
