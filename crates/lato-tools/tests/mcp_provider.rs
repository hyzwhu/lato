//! Progressive discovery provider tests (Phase 6C Task 4).

use async_trait::async_trait;
use lato_core::{
    PolicyMode, SandboxProfile, SessionId, ToolCallId, ToolContext, ToolError, TurnId,
};
use lato_mcp::{
    McpDescriptorSet, McpManager, McpSearchHit, McpServerSpec, McpToolDescriptor, McpTransportKind,
};
use lato_policy::{ApprovalLedger, PolicyEngine};
use lato_tools::{
    BuiltinToolEnvironment, McpManagerBackend, McpProviderConfig, McpToolBackend, PolicyScope,
    ToolRuntimeBuilder, mcp_provider_tools,
};
use lato_workspace::{FileLocks, SessionTrust};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct RecordingBackend {
    hits: Mutex<Vec<McpSearchHit>>,
    calls: AtomicUsize,
    last_call: Mutex<Option<(String, String, Value)>>,
}

impl RecordingBackend {
    fn with_hits(hits: Vec<McpSearchHit>) -> Self {
        Self {
            hits: Mutex::new(hits),
            calls: AtomicUsize::new(0),
            last_call: Mutex::new(None),
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
            .filter(|hit| {
                hit.qualified_name.to_ascii_lowercase().contains(&needle)
                    || hit.description.to_ascii_lowercase().contains(&needle)
            })
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
            .filter(|hit| servers.iter().any(|server| server == &hit.server))
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
        _context: &ToolContext,
        server: &str,
        name: &str,
        arguments: Value,
    ) -> Result<Value, ToolError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        *self.last_call.lock().unwrap() = Some((server.to_owned(), name.to_owned(), arguments));
        Ok(json!({
            "content": [{"type": "text", "text": "{\"ok\":true}"}],
            "isError": false
        }))
    }
}

fn sample_hit() -> McpSearchHit {
    McpSearchHit {
        server: "demo".into(),
        name: "ping".into(),
        qualified_name: "demo__ping".into(),
        description: "Ping the fixture server".into(),
        input_schema: json!({"type": "object", "properties": {"n": {"type": "integer"}}}),
    }
}

fn context(call_id: &str) -> ToolContext {
    ToolContext {
        session_id: SessionId::from("session-mcp"),
        turn_id: TurnId::from("turn-mcp"),
        call_id: ToolCallId::from(call_id),
        cancellation: CancellationToken::new(),
        execution_grant: None,
    }
}

fn runtime_with_mcp(
    root: &Path,
    backend: Arc<dyn McpToolBackend>,
    config: McpProviderConfig,
    trusted: bool,
) -> Arc<lato_tools::ToolRuntime> {
    let trust = if trusted {
        SessionTrust::for_headless_prompt(root)
    } else {
        SessionTrust::for_interactive(root, false)
    };
    let scope = PolicyScope {
        workspace_root: root.to_path_buf(),
        mode: match trust.mode {
            lato_workspace::ApprovalMode::Ask => PolicyMode::Ask,
            lato_workspace::ApprovalMode::Auto => PolicyMode::Auto,
            lato_workspace::ApprovalMode::Always => PolicyMode::Always,
        },
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
    builder.register_mcp_provider(backend, &config).unwrap();
    Arc::new(builder.build().unwrap())
}

fn model_names(runtime: &lato_tools::ToolRuntime) -> Vec<String> {
    runtime
        .model_definitions()
        .into_iter()
        .filter_map(|definition| {
            definition
                .pointer("/function/name")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

#[test]
fn default_model_definitions_exclude_raw_mcp_tools() {
    let root = tempfile::tempdir().unwrap();
    let backend: Arc<dyn McpToolBackend> = Arc::new(RecordingBackend::with_hits(vec![
        sample_hit(),
        McpSearchHit {
            server: "other".into(),
            name: "secret".into(),
            qualified_name: "other__secret".into(),
            description: "hidden".into(),
            input_schema: json!({"type": "object"}),
        },
    ]));
    let runtime = runtime_with_mcp(
        root.path(),
        backend,
        McpProviderConfig::default(),
        true,
    );
    let names = model_names(&runtime);
    assert!(names.contains(&"search_tool".to_owned()));
    assert!(names.contains(&"use_tool".to_owned()));
    assert!(names.contains(&"read_file".to_owned()));
    assert!(
        !names.iter().any(|name| name.contains("__")),
        "raw MCP tools must stay out of default model_definitions: {names:?}"
    );
}

#[test]
fn direct_expand_allowlist_exposes_only_configured_servers() {
    let root = tempfile::tempdir().unwrap();
    let backend: Arc<dyn McpToolBackend> = Arc::new(RecordingBackend::with_hits(vec![
        sample_hit(),
        McpSearchHit {
            server: "other".into(),
            name: "secret".into(),
            qualified_name: "other__secret".into(),
            description: "hidden".into(),
            input_schema: json!({"type": "object"}),
        },
    ]));
    let runtime = runtime_with_mcp(
        root.path(),
        backend,
        McpProviderConfig {
            direct_expand_servers: vec!["demo".into()],
            ..McpProviderConfig::default()
        },
        true,
    );
    let names = model_names(&runtime);
    assert!(names.contains(&"demo__ping".to_owned()));
    assert!(!names.contains(&"other__secret".to_owned()));
}

#[tokio::test]
async fn direct_use_tool_invocation_requires_policy_grant() {
    let backend = Arc::new(RecordingBackend::with_hits(vec![sample_hit()]));
    let tools = mcp_provider_tools(backend.clone(), &McpProviderConfig::default());
    let use_tool = tools
        .into_iter()
        .find(|tool| tool.descriptor().name.local_name() == "use_tool")
        .unwrap();
    let error = use_tool
        .invoke(
            context("direct"),
            json!({"tool": "demo__ping", "arguments": {}}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "policy.grant_missing");
    assert_eq!(backend.calls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn untrusted_project_denies_use_tool_before_backend_call() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(RecordingBackend::with_hits(vec![sample_hit()]));
    let runtime = runtime_with_mcp(
        root.path(),
        backend.clone(),
        McpProviderConfig::default(),
        false,
    );
    let error = runtime
        .invoke(
            context("denied"),
            "use_tool",
            json!({"tool": "demo__ping", "arguments": {}}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "policy.untrusted_extension");
    assert_eq!(backend.calls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn search_tool_and_use_tool_work_through_runtime() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(RecordingBackend::with_hits(vec![sample_hit()]));
    let runtime = runtime_with_mcp(
        root.path(),
        backend.clone(),
        McpProviderConfig::default(),
        true,
    );

    let search = runtime
        .invoke(context("search"), "search_tool", json!({"query": "ping"}))
        .await
        .unwrap();
    assert_eq!(search.metadata["kind"], "mcp_search");
    assert_eq!(search.metadata["count"], 1);
    let payload: Value = serde_json::from_str(&search.content).unwrap();
    assert_eq!(payload["matches"][0]["qualified_name"], "demo__ping");

    let used = runtime
        .invoke(
            context("use"),
            "use_tool",
            json!({
                "server": "demo",
                "name": "ping",
                "arguments": {"n": 1}
            }),
        )
        .await
        .unwrap();
    assert_eq!(used.content, "{\"ok\":true}");
    assert_eq!(used.metadata["kind"], "mcp_tool_result");
    assert_eq!(used.metadata["qualifiedName"], "demo__ping");
    assert_eq!(backend.calls.load(Ordering::Acquire), 1);
    let last = backend.last_call.lock().unwrap().clone().unwrap();
    assert_eq!(last.0, "demo");
    assert_eq!(last.1, "ping");
    assert_eq!(last.2, json!({"n": 1}));
}

fn write_executable(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    path
}

fn call_stdio_script() -> &'static str {
    r#"#!/usr/bin/env python3
import json, sys
def reply(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        reply({
            "jsonrpc": "2.0",
            "id": msg["id"],
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fixture-call", "version": "1.0.0"},
            },
        })
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        reply({
            "jsonrpc": "2.0",
            "id": msg["id"],
            "result": {
                "tools": [{
                    "name": "ping",
                    "description": "returns fixed JSON",
                    "inputSchema": {"type": "object", "properties": {"msg": {"type": "string"}}},
                }]
            },
        })
    elif method == "tools/call":
        params = msg.get("params") or {}
        arguments = params.get("arguments") or {}
        reply({
            "jsonrpc": "2.0",
            "id": msg["id"],
            "result": {
                "content": [{"type": "text", "text": json.dumps({"pong": arguments.get("msg", "")})}],
                "isError": False,
            },
        })
    elif "id" in msg:
        reply({"jsonrpc": "2.0", "id": msg["id"], "result": {"ok": True}})
"#
}

#[cfg(unix)]
#[tokio::test]
async fn manager_backed_search_and_use_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_executable(dir.path(), "server.py", call_stdio_script());
    let cancel = CancellationToken::new();
    let manager = Arc::new(McpManager::new(
        Arc::new(McpDescriptorSet {
            generation: 7,
            servers: Arc::from([McpServerSpec {
                id: "fixture/demo".into(),
                plugin_name: "fixture".into(),
                server_name: "demo".into(),
                transport: McpTransportKind::Stdio,
                command: Some(PathBuf::from("python3")),
                args: vec![script.display().to_string()],
                env: BTreeMap::new(),
                cwd: None,
                url: None,
                headers: Vec::new(),
                timeout_ms: 5_000,
                source_dir: PathBuf::from("."),
            }]),
            diagnostics: Arc::from([]),
            allowed_tools: None,
        }),
        cancel.clone(),
    ));
    let backend: Arc<dyn McpToolBackend> = Arc::new(McpManagerBackend::new(manager.clone()));
    let runtime = runtime_with_mcp(
        dir.path(),
        backend,
        McpProviderConfig::default(),
        true,
    );

    let names = model_names(&runtime);
    assert!(names.contains(&"search_tool".to_owned()));
    assert!(names.contains(&"use_tool".to_owned()));
    assert!(!names.iter().any(|name| name.contains("__")));

    let search = runtime
        .invoke(context("s1"), "search_tool", json!({"query": "ping"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&search.content).unwrap();
    assert_eq!(payload["count"], 1);
    assert_eq!(payload["matches"][0]["qualified_name"], "demo__ping");

    let used = runtime
        .invoke(
            context("u1"),
            "use_tool",
            json!({"tool": "demo__ping", "arguments": {"msg": "hi"}}),
        )
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&used.content).unwrap();
    assert_eq!(body, json!({"pong": "hi"}));
    assert_eq!(used.metadata["isError"], false);

    let _ = manager
        .shutdown_all(std::time::Instant::now() + Duration::from_secs(2))
        .await;
}


#[tokio::test]
async fn oversized_mcp_result_is_truncated_and_spilled() {
    let dir = tempfile::tempdir().unwrap();
    let huge = "Z".repeat(lato_tools::TOOL_OUTPUT_LIMIT_BYTES + 2_500);
    struct HugeBackend {
        inner: RecordingBackend,
        body: String,
    }
    #[async_trait]
    impl McpToolBackend for HugeBackend {
        async fn ensure_index(&self) -> Result<(), ToolError> {
            Ok(())
        }
        fn search(&self, query: &str) -> Vec<McpSearchHit> {
            self.inner.search(query)
        }
        fn lookup(&self, qualified_or_parts: &str) -> Option<McpToolDescriptor> {
            self.inner.lookup(qualified_or_parts)
        }
        fn tools_for_servers(&self, servers: &[String]) -> Vec<McpToolDescriptor> {
            self.inner.tools_for_servers(servers)
        }
        fn generation(&self) -> u64 {
            42
        }
        async fn call_tool(
            &self,
            _context: &ToolContext,
            _server: &str,
            _name: &str,
            _arguments: Value,
        ) -> Result<Value, ToolError> {
            Ok(json!({
                "content": [{"type": "text", "text": self.body}],
                "isError": false
            }))
        }
    }
    let backend: Arc<dyn McpToolBackend> = Arc::new(HugeBackend {
        inner: RecordingBackend::with_hits(vec![McpSearchHit {
            server: "demo".into(),
            name: "blob".into(),
            qualified_name: "demo__blob".into(),
            description: "returns a huge blob".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }]),
        body: huge.clone(),
    });
    let runtime = runtime_with_mcp(
        dir.path(),
        backend,
        McpProviderConfig {
            workspace_root: dir.path().to_path_buf(),
            ..McpProviderConfig::default()
        },
        true,
    );
    let context = ToolContext {
        session_id: SessionId::from("session-mcp"),
        turn_id: TurnId::from("turn-mcp"),
        call_id: ToolCallId::from("call-big"),
        cancellation: CancellationToken::new(),
        execution_grant: None,
    };
    let prepared = runtime
        .prepare(
            context,
            "use_tool",
            json!({"tool": "demo__blob", "arguments": {}}),
        )
        .unwrap();
    let output = runtime
        .execute_without_approval_for_test(prepared)
        .await
        .unwrap();
    assert!(output.truncated, "expected truncated flag");
    let artifact = output.artifact_path.expect("spill path");
    assert!(artifact.contains(".lato/tool-output/"));
    assert_eq!(std::fs::read_to_string(&artifact).unwrap(), huge);
    assert!(output.content.len() < huge.len());
    assert!(output.content.contains("[tool output truncated"));
    assert!(!output.content.contains(&"Z".repeat(lato_tools::TOOL_OUTPUT_LIMIT_BYTES + 2_500)));
    assert_eq!(output.metadata["kind"], "mcp_tool_result");
    assert_eq!(output.metadata["generation"], 42);
    assert!(output.metadata["argsHash"].as_str().unwrap().starts_with("sha256:"));
    assert!(output.metadata["resultHash"].as_str().unwrap().starts_with("sha256:"));
    // Journal-facing metadata must not embed the full body or secrets.
    let meta = output.metadata.to_string();
    assert!(!meta.contains(&"Z".repeat(100)));
}

#[test]
fn mcp_error_mapping_uses_stable_codes_without_secrets() {
    let err = lato_mcp::McpError::UnsafeUrl;
    let mapped = {
        // Reuse provider mapping via a tiny call through NotRunning etc.
        let code = err.code();
        let message = err.safe_message();
        (code, message)
    };
    assert_eq!(mapped.0, "mcp.unsafe_url");
    assert!(!mapped.1.contains("secret"));
    assert_eq!(lato_mcp::McpError::Timeout { timeout_ms: 9 }.code(), "mcp.timeout");
    assert_eq!(lato_mcp::McpError::rpc(1, "x").code(), "mcp.rpc_error");
}
