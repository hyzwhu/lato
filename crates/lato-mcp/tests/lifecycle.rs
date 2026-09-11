//! Bounded MCP server lifecycle & transport fixtures (Phase 6C Task 2).

use std::{
    collections::BTreeMap,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use lato_mcp::{
    McpDescriptorSet, McpDnsResolver, McpError, McpManager, McpServerSpec, McpTransportKind,
    initialize, pid_alive, redact_url_credentials, shutdown_server, start_server,
    start_server_with_resolver, stdio_residue_alive, validate_mcp_url,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};
use tokio_util::sync::CancellationToken;
use url::Url;

fn stdio_spec(name: &str, command: PathBuf, args: Vec<String>, timeout_ms: u64) -> McpServerSpec {
    McpServerSpec {
        id: format!("fixture/{name}"),
        plugin_name: "fixture".into(),
        server_name: name.into(),
        transport: McpTransportKind::Stdio,
        command: Some(command),
        args,
        env: BTreeMap::new(),
        cwd: None,
        url: None,
        headers: Vec::new(),
        timeout_ms,
        source_dir: PathBuf::from("."),
    }
}

fn http_spec(name: &str, url: Url, timeout_ms: u64) -> McpServerSpec {
    McpServerSpec {
        id: format!("fixture/{name}"),
        plugin_name: "fixture".into(),
        server_name: name.into(),
        transport: McpTransportKind::StreamableHttp,
        command: None,
        args: Vec::new(),
        env: BTreeMap::new(),
        cwd: None,
        url: Some(url),
        headers: Vec::new(),
        timeout_ms,
        source_dir: PathBuf::from("."),
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

/// Persistent MCP stdio fixture: responds to initialize and keeps running until stdin closes.
fn mcp_stdio_server_script() -> &'static str {
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
                "serverInfo": {"name": "fixture-stdio", "version": "1.0.0"},
            },
        })
    elif method == "notifications/initialized":
        pass
    elif "id" in msg:
        reply({"jsonrpc": "2.0", "id": msg["id"], "result": {"ok": True}})
"#
}

fn hang_stdio_script() -> &'static str {
    r#"#!/bin/sh
# Ignore TERM briefly so the parent's process-group kill path is exercised.
trap '' TERM
sleep 60
"#
}

fn cancel_stdio_script() -> &'static str {
    r#"#!/usr/bin/env python3
import sys, time
# Read one line then hang without answering — cancel should reap us.
sys.stdin.readline()
time.sleep(60)
"#
}

fn panic_stdio_script() -> &'static str {
    r#"#!/bin/sh
# Exit immediately to simulate an isolated server crash.
exit 42
"#
}

struct FixedResolver {
    addrs: Vec<SocketAddr>,
}

#[async_trait]
impl McpDnsResolver for FixedResolver {
    async fn resolve(&self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(self.addrs.clone())
    }
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_start_initialize_shutdown_leaves_no_child() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_executable(dir.path(), "server.py", mcp_stdio_server_script());
    let spec = stdio_spec("demo", script, Vec::new(), 5_000);
    let cancel = CancellationToken::new();
    let mut handle = start_server(&spec, 1, cancel.clone()).await.unwrap();
    let pid = handle.stdio_pid().expect("stdio pid");
    assert!(pid_alive(pid));
    let init = initialize(&mut handle).await.unwrap();
    assert_eq!(init.server_info.name, "fixture-stdio");
    assert_eq!(init.protocol_version, "2024-11-05");
    let deadline = Instant::now() + Duration::from_secs(2);
    shutdown_server(handle, deadline).await.unwrap();
    // Allow a short grace for reaping.
    for _ in 0..50 {
        if !stdio_residue_alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !stdio_residue_alive(pid),
        "stdio child/process-group still alive after shutdown"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_timeout_kills_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_executable(dir.path(), "hang.sh", hang_stdio_script());
    // Spawn a child inside the process group so we can verify PG kill.
    let wrapper = write_executable(
        dir.path(),
        "wrapper.sh",
        &format!(
            "#!/bin/sh\n{} &\nexec {}\n",
            script.display(),
            script.display()
        ),
    );
    let spec = stdio_spec("hang", wrapper, Vec::new(), 200);
    let cancel = CancellationToken::new();
    let mut handle = start_server(&spec, 1, cancel).await.unwrap();
    let pid = handle.stdio_pid().unwrap();
    let err = initialize(&mut handle).await.unwrap_err();
    assert!(matches!(err, McpError::Timeout { .. }), "got {err:?}");
    for _ in 0..50 {
        if !stdio_residue_alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !stdio_residue_alive(pid),
        "hanging process group survived timeout"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_cancel_mid_call_reaps_child() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_executable(dir.path(), "cancel.py", cancel_stdio_script());
    let spec = stdio_spec("cancel", script, Vec::new(), 30_000);
    let cancel = CancellationToken::new();
    let mut handle = start_server(&spec, 1, cancel.clone()).await.unwrap();
    let pid = handle.stdio_pid().unwrap();
    let init = tokio::spawn(async move { initialize(&mut handle).await });
    // Let the request land, then cancel.
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let err = init.await.unwrap().unwrap_err();
    assert!(matches!(err, McpError::Cancelled), "got {err:?}");
    // initialize moved handle into the task — we only have pid.
    for _ in 0..50 {
        if !stdio_residue_alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !stdio_residue_alive(pid),
        "cancelled stdio child still alive"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn manager_isolates_panicking_server() {
    let dir = tempfile::tempdir().unwrap();
    let good = write_executable(dir.path(), "good.py", mcp_stdio_server_script());
    let bad = write_executable(dir.path(), "bad.sh", panic_stdio_script());
    let servers = vec![
        stdio_spec("good", good, Vec::new(), 5_000),
        stdio_spec("bad", bad, Vec::new(), 5_000),
    ];
    let descriptors = Arc::new(McpDescriptorSet {
        generation: 7,
        servers: servers.into(),
        diagnostics: Arc::from([]),
        allowed_tools: None,
    });
    let manager = McpManager::new(descriptors, CancellationToken::new());
    let bad_err = manager.initialize_server("bad").await.unwrap_err();
    assert!(
        matches!(
            bad_err,
            McpError::Io
                | McpError::Unhealthy
                | McpError::Protocol { .. }
                | McpError::Timeout { .. }
        ),
        "unexpected {bad_err:?}"
    );
    let _ = manager.mark_unhealthy_and_reap("bad").await;
    let good_init = manager.initialize_server("good").await.unwrap();
    assert_eq!(good_init.server_info.name, "fixture-stdio");
    assert!(manager.health("good").await.unwrap());
    manager
        .shutdown_all(Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
}

async fn spawn_json_rpc_http_server(
    handler: impl Fn(serde_json::Value) -> (u16, String) + Send + Sync + 'static,
) -> (Url, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut socket, _)) = accepted else { break };
                    let handler = Arc::clone(&handler);
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 8192];
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
                        let value: serde_json::Value =
                            serde_json::from_str(body).unwrap_or(serde_json::json!({}));
                        let (status, response_body) = handler(value);
                        let response = format!(
                            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                            response_body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            }
        }
    });
    let url = Url::parse(&format!("http://{addr}/mcp")).unwrap();
    (url, shutdown_tx)
}

#[tokio::test]
async fn http_loopback_initialize_ok() {
    let (url, shutdown) = spawn_json_rpc_http_server(|value| {
        let id = value.get("id").cloned().unwrap_or(serde_json::json!(1));
        if value.get("method").and_then(|m| m.as_str()) == Some("initialize") {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {"name": "fixture-http", "version": "1.0.0"},
                }
            });
            (200, body.to_string())
        } else {
            (202, String::new())
        }
    })
    .await;
    let spec = http_spec("http-demo", url, 5_000);
    let cancel = CancellationToken::new();
    let mut handle = start_server(&spec, 1, cancel).await.unwrap();
    let init = initialize(&mut handle).await.unwrap();
    assert_eq!(init.server_info.name, "fixture-http");
    shutdown_server(handle, Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    let _ = shutdown.send(());
}

#[tokio::test]
async fn http_private_ip_blocked() {
    let url = Url::parse("http://evil.example/mcp").unwrap();
    let resolver = FixedResolver {
        addrs: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)), 80)],
    };
    let err = validate_mcp_url(&url, &resolver).await.unwrap_err();
    assert!(matches!(err, McpError::UnsafeUrl));

    let https = Url::parse("https://evil.example/mcp").unwrap();
    let err = validate_mcp_url(&https, &resolver).await.unwrap_err();
    assert!(matches!(err, McpError::UnsafeUrl));

    let spec = http_spec("blocked", url, 1_000);
    let started = start_server_with_resolver(&spec, 1, CancellationToken::new(), &resolver).await;
    assert!(matches!(started, Err(McpError::UnsafeUrl)));
}

#[tokio::test]
async fn http_redirect_not_followed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = socket.read(&mut buf).await;
        let response = "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = socket.write_all(response.as_bytes()).await;
    });
    let url = Url::parse(&format!("http://{addr}/mcp")).unwrap();
    let spec = http_spec("redirect", url, 5_000);
    let mut handle = start_server(&spec, 1, CancellationToken::new())
        .await
        .unwrap();
    let err = initialize(&mut handle).await.unwrap_err();
    assert!(matches!(err, McpError::Http), "got {err:?}");
}

#[tokio::test]
async fn http_errors_redact_credentials() {
    let url = Url::parse("https://user:super-secret@example.com/mcp").unwrap();
    let redacted = redact_url_credentials(&url);
    assert!(!redacted.contains("super-secret"));
    assert!(!redacted.contains("user:"));
    // Credential-bearing URLs are rejected before dial.
    let err = validate_mcp_url(&url, &SystemResolver).await.unwrap_err();
    assert!(matches!(err, McpError::UnsafeUrl));
    let display = format!("{err}");
    assert!(!display.contains("super-secret"));
}

struct SystemResolver;

#[async_trait]
impl McpDnsResolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }
}

#[tokio::test]
async fn manager_call_tool_is_transport_only_not_model_facing() {
    // McpManager::call_tool is a transport RPC for lato-tools providers.
    // Model-facing delivery must still go through ToolRuntime (search_tool/use_tool).
    let manager = McpManager::new(McpDescriptorSet::empty(1), CancellationToken::new());
    assert_eq!(manager.generation(), 1);
    assert!(manager.running_servers().await.is_empty());
    assert!(manager.cache().is_empty());
    let err = manager
        .call_tool("missing", "noop", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, lato_mcp::McpError::NotRunning(_)));
}
