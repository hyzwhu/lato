//! Phase 6C installed-command / library smoke for MCP runtime.
//!
//! Proves (against the installed `lato` binary when `LATO_SMOKE_BINARY` is set):
//! trusted temp plugin with stdio + loopback streamable HTTP, search→use,
//! SSRF reject, SessionEnd reap, and child capability narrowing.
//! Emits `phase6c-mcp-smoke-ok` on success.

use std::{
    collections::BTreeSet,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use lato_agent::SessionMcpHandle;
use lato_core::{PolicyMode, SandboxProfile, SessionId, ToolCallId, ToolContext, TurnId};
use lato_extensions::{
    CapabilityCeiling, DiscoveryConfig, McpCapabilityCeiling, PluginConfig, PluginSnapshot,
    build_snapshot, discover_plugins, materialize_mcp,
};
use lato_mcp::{
    McpDnsResolver, McpError, McpManager, McpTransportKind, qualify_tool, stdio_residue_alive,
    validate_mcp_url,
};
use lato_policy::{ApprovalLedger, PolicyEngine};
use lato_tools::{
    BuiltinToolEnvironment, McpProviderConfig, McpToolBackend, PolicyScope, ToolRuntime,
    ToolRuntimeBuilder,
};
use lato_workspace::{FileLocks, SessionTrust};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};
use tokio_util::sync::CancellationToken;
use url::Url;

struct FixedResolver {
    addrs: Vec<SocketAddr>,
}

#[async_trait]
impl McpDnsResolver for FixedResolver {
    async fn resolve(&self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(self.addrs.clone())
    }
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

fn stdio_ping_script() -> &'static str {
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
    mid = msg.get("id")
    if method == "initialize":
        reply({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "smoke-stdio", "version": "1.0.0"},
            },
        })
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        reply({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "tools": [{
                    "name": "ping",
                    "description": "Phase 6C smoke ping",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"n": {"type": "integer"}},
                        "additionalProperties": False
                    },
                }]
            },
        })
    elif method == "tools/call":
        args = (msg.get("params") or {}).get("arguments") or {}
        reply({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "content": [{"type": "text", "text": json.dumps({"pong": args.get("n", 0)})}],
                "isError": False,
            },
        })
    elif mid is not None:
        reply({"jsonrpc": "2.0", "id": mid, "result": {"ok": True}})
"#
}

async fn spawn_http_mcp_server() -> (Url, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut socket, _)) = accepted else { break };
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 16384];
                        let n = socket.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        let request = String::from_utf8_lossy(&buf[..n]);
                        let body = request
                            .split("\r\n\r\n")
                            .nth(1)
                            .unwrap_or("")
                            .trim_end_matches('\0');
                        let value: Value =
                            serde_json::from_str(body).unwrap_or_else(|_| json!({}));
                        let id = value.get("id").cloned().unwrap_or(json!(1));
                        let method = value.get("method").and_then(Value::as_str).unwrap_or("");
                        let (status, response_body) = match method {
                            "initialize" => (
                                200,
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "protocolVersion": "2024-11-05",
                                        "capabilities": {"tools": {}},
                                        "serverInfo": {"name": "smoke-http", "version": "1.0.0"},
                                    }
                                })
                                .to_string(),
                            ),
                            "notifications/initialized" => (202, String::new()),
                            "tools/list" => (
                                200,
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "tools": [{
                                            "name": "echo",
                                            "description": "Phase 6C smoke echo",
                                            "inputSchema": {
                                                "type": "object",
                                                "properties": {"text": {"type": "string"}},
                                                "additionalProperties": false
                                            }
                                        }]
                                    }
                                })
                                .to_string(),
                            ),
                            "tools/call" => {
                                let args = value
                                    .pointer("/params/arguments")
                                    .cloned()
                                    .unwrap_or(json!({}));
                                let text = args
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or("missing");
                                (
                                    200,
                                    json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": {
                                            "content": [{"type": "text", "text": text}],
                                            "isError": false
                                        }
                                    })
                                    .to_string(),
                                )
                            }
                            _ => (
                                200,
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "error": {"code": -32601, "message": "method not found"}
                                })
                                .to_string(),
                            ),
                        };
                        let response = if response_body.is_empty() {
                            format!(
                                "HTTP/1.1 {status} Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                        } else {
                            format!(
                                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                                response_body.len()
                            )
                        };
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            }
        }
    });
    let url = Url::parse(&format!("http://{addr}/mcp")).unwrap();
    (url, shutdown_tx)
}

fn context(call_id: &str) -> ToolContext {
    ToolContext {
        session_id: SessionId::from("phase6c-mcp-smoke"),
        turn_id: TurnId::from("turn-1"),
        call_id: ToolCallId::from(call_id),
        cancellation: CancellationToken::new(),
        execution_grant: None,
    }
}

fn runtime_with_mcp(root: &Path, backend: Arc<dyn McpToolBackend>) -> Arc<ToolRuntime> {
    let trust = SessionTrust::for_headless_prompt(root);
    let scope = PolicyScope {
        workspace_root: root.to_path_buf(),
        mode: PolicyMode::Always,
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

fn build_trusted_plugin_snapshot(
    root: &Path,
    stdio_script: &Path,
    http_url: &Url,
) -> Arc<PluginSnapshot> {
    let workspace = root.join("workspace");
    let home = root.join("home");
    let plugin = root.join("smoke-plugin");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(
        plugin.join("plugin.json"),
        json!({
            "name": "smoke-plugin",
            "mcpServers": {
                "demo": {
                    "command": "python3",
                    "args": [stdio_script.file_name().unwrap().to_string_lossy()],
                    "cwd": "."
                },
                "httpdemo": {
                    "url": http_url.as_str(),
                    "transport": "streamable-http"
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    // Keep the stdio script inside the plugin root for path confinement.
    std::fs::copy(stdio_script, plugin.join(stdio_script.file_name().unwrap())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            plugin.join(stdio_script.file_name().unwrap()),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    build_snapshot(
        61,
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

fn pgrep_script(script_name: &str) -> Vec<u32> {
    let output = Command::new("pgrep")
        .args(["-f", script_name])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

#[test]
fn phase6c_installed_command_smoke_exercises_mcp_runtime() {
    let binary = std::env::var_os("LATO_SMOKE_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_lato")));
    let version = Command::new(&binary).arg("--version").output().unwrap();
    assert!(
        version.status.success(),
        "lato --version failed via {}: {}",
        binary.display(),
        String::from_utf8_lossy(&version.stderr)
    );
    let version_text = String::from_utf8_lossy(&version.stdout);
    assert!(
        version_text.contains("lato"),
        "unexpected version output: {version_text}"
    );

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let root = tempfile::tempdir().unwrap();
        let script = write_executable(root.path(), "smoke_stdio_server.py", stdio_ping_script());
        let (http_url, http_shutdown) = spawn_http_mcp_server().await;
        let snapshot = build_trusted_plugin_snapshot(root.path(), &script, &http_url);
        let set = materialize_mcp(&snapshot);
        assert_eq!(set.generation, 61);
        assert_eq!(set.servers.len(), 2);
        assert!(
            set.servers
                .iter()
                .any(|s| s.server_name == "demo" && s.transport == McpTransportKind::Stdio)
        );
        assert!(set.servers.iter().any(|s| {
            s.server_name == "httpdemo" && s.transport == McpTransportKind::StreamableHttp
        }));

        let cancel = CancellationToken::new();
        let handle = SessionMcpHandle::empty();
        let manager = handle
            .install_from_snapshot(&snapshot, cancel.clone())
            .await;
        assert_eq!(handle.snapshot_generation().await, 61);

        // Progressive discovery surface: only search_tool / use_tool (plus builtins).
        let tool_runtime = runtime_with_mcp(root.path(), Arc::new(handle.clone()));
        let names: Vec<_> = tool_runtime
            .model_definitions()
            .into_iter()
            .filter_map(|d| {
                d.pointer("/function/name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
        assert!(names.contains(&"search_tool".to_owned()));
        assert!(names.contains(&"use_tool".to_owned()));
        assert!(
            !names.iter().any(|n| n.contains("__")),
            "direct MCP expand must stay opt-in: {names:?}"
        );

        // search → use (stdio ping)
        let search = tool_runtime
            .invoke(
                context("search-stdio"),
                "search_tool",
                json!({"query": "ping"}),
            )
            .await
            .unwrap();
        let search_text = search.content.to_string();
        assert!(
            search_text.contains("demo__ping")
                || search.metadata.to_string().contains("demo__ping"),
            "search_tool missed stdio ping: content={} metadata={}",
            search.content,
            search.metadata
        );

        let use_stdio = tool_runtime
            .invoke(
                context("use-stdio"),
                "use_tool",
                json!({"tool": "demo__ping", "arguments": {"n": 7}}),
            )
            .await
            .unwrap();
        let stdio_blob = format!("{}{}", use_stdio.content, use_stdio.metadata);
        assert!(
            stdio_blob.contains("7") || stdio_blob.contains("pong"),
            "stdio use_tool missing pong payload: {stdio_blob}"
        );
        assert_eq!(use_stdio.metadata["kind"], "mcp_tool_result");

        // search → use (loopback HTTP echo)
        let search_http = tool_runtime
            .invoke(
                context("search-http"),
                "search_tool",
                json!({"query": "echo"}),
            )
            .await
            .unwrap();
        let http_search_blob = format!("{}{}", search_http.content, search_http.metadata);
        assert!(
            http_search_blob.contains("httpdemo__echo"),
            "search_tool missed HTTP echo: {http_search_blob}"
        );
        let use_http = tool_runtime
            .invoke(
                context("use-http"),
                "use_tool",
                json!({"tool": "httpdemo__echo", "arguments": {"text": "phase6c-http-ok"}}),
            )
            .await
            .unwrap();
        let http_blob = format!("{}{}", use_http.content, use_http.metadata);
        assert!(
            http_blob.contains("phase6c-http-ok"),
            "HTTP use_tool missing echo payload: {http_blob}"
        );

        // SSRF reject sample (private / link-local after resolution).
        let private = Url::parse("https://evil.example/mcp").unwrap();
        let resolver = FixedResolver {
            addrs: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)), 443)],
        };
        let err = validate_mcp_url(&private, &resolver).await.unwrap_err();
        assert!(matches!(err, McpError::UnsafeUrl));
        assert_eq!(err.code(), "mcp.unsafe_url");
        let link_local = FixedResolver {
            addrs: vec![SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
                80,
            )],
        };
        let err = validate_mcp_url(&Url::parse("http://meta.example/mcp").unwrap(), &link_local)
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::UnsafeUrl));

        // Child narrowing: child cannot restore removed server/tool.
        let child = snapshot.derive_child(&CapabilityCeiling {
            parent: vec![lato_core::ToolCapability::ExtensionInvoke],
            profile: vec![lato_core::ToolCapability::ExtensionInvoke],
            workspace: vec![lato_core::ToolCapability::ExtensionInvoke],
            mcp: McpCapabilityCeiling {
                allowed_servers: Some(BTreeSet::from(["demo".into()])),
                allowed_tools: Some(BTreeSet::from([qualify_tool("demo", "ping")])),
            },
            workflows: Default::default(),
        });
        let child_set = materialize_mcp(&child);
        assert_eq!(child_set.servers.len(), 1);
        assert_eq!(child_set.servers[0].server_name, "demo");
        assert!(child_set.allows_tool(&qualify_tool("demo", "ping")));
        assert!(!child_set.allows_tool(&qualify_tool("httpdemo", "echo")));
        let child_manager = McpManager::new(child_set, CancellationToken::new());
        let denied = child_manager
            .call_tool("httpdemo", "echo", json!({"text": "nope"}))
            .await
            .unwrap_err();
        assert_eq!(denied.code(), "mcp.capability_denied");
        let restored = child.derive_child(&CapabilityCeiling {
            parent: vec![lato_core::ToolCapability::ExtensionInvoke],
            profile: vec![lato_core::ToolCapability::ExtensionInvoke],
            workspace: vec![lato_core::ToolCapability::ExtensionInvoke],
            mcp: McpCapabilityCeiling {
                allowed_servers: Some(BTreeSet::from(["demo".into(), "httpdemo".into()])),
                allowed_tools: Some(BTreeSet::from([
                    qualify_tool("demo", "ping"),
                    qualify_tool("httpdemo", "echo"),
                ])),
            },
            workflows: Default::default(),
        });
        let restored_set = materialize_mcp(&restored);
        assert!(!restored_set.allows_tool(&qualify_tool("httpdemo", "echo")));

        // Capture stdio pids before SessionEnd shutdown.
        assert!(manager.running_servers().await.contains(&"demo".to_owned()));
        let before = pgrep_script("smoke_stdio_server.py");
        assert!(
            !before.is_empty(),
            "expected live stdio MCP child before SessionEnd"
        );

        // SessionEnd reap
        handle
            .shutdown(Instant::now() + Duration::from_secs(2))
            .await;
        assert!(handle.manager().await.is_none());
        for _ in 0..50 {
            let alive = before.iter().any(|pid| stdio_residue_alive(*pid));
            if !alive && pgrep_script("smoke_stdio_server.py").is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        for pid in &before {
            assert!(
                !stdio_residue_alive(*pid),
                "stdio MCP pid {pid} still alive after SessionEnd"
            );
        }
        assert!(
            pgrep_script("smoke_stdio_server.py").is_empty(),
            "stdio MCP script still matched by pgrep after SessionEnd"
        );

        let _ = http_shutdown.send(());
    });

    println!("phase6c-mcp-smoke-ok");
}
