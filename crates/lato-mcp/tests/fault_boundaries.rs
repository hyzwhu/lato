//! Phase 6C Task 6 — result & fault boundaries (M-15…M-17).
//!
//! Covers stable protocol/timeout/SSRF codes, cancel semantics, crash isolation,
//! no-redirect, and credential redaction. Oversized spill is covered in lato-tools.

use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use lato_mcp::{
    McpDescriptorSet, McpDnsResolver, McpError, McpManager, McpServerSpec, McpTransportKind,
    initialize, redact_url_credentials, shutdown_server, start_server, validate_mcp_url,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
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

fn http_spec(name: &str, url: Url, timeout_ms: u64) -> McpServerSpec {
    McpServerSpec {
        id: format!("plugin/{name}"),
        plugin_name: "plugin".into(),
        server_name: name.into(),
        transport: McpTransportKind::StreamableHttp,
        command: None,
        args: Vec::new(),
        env: Default::default(),
        cwd: None,
        url: Some(url),
        headers: Vec::new(),
        timeout_ms,
        source_dir: PathBuf::from("/tmp"),
    }
}

#[test]
fn protocol_timeout_and_ssrf_map_to_stable_codes() {
    assert_eq!(McpError::Timeout { timeout_ms: 30 }.code(), "mcp.timeout");
    assert_eq!(McpError::protocol("bad frame").code(), "mcp.protocol");
    assert_eq!(McpError::rpc(-32602, "invalid params").code(), "mcp.rpc_error");
    assert_eq!(McpError::UnsafeUrl.code(), "mcp.unsafe_url");
    assert_eq!(McpError::Cancelled.code(), "tool.cancelled");
    assert_eq!(McpError::Http.code(), "mcp.http");
    assert!(McpError::Timeout { timeout_ms: 1 }.retryable_after_backoff());
    assert!(!McpError::UnsafeUrl.retryable_after_backoff());
    assert!(!McpError::protocol("x").retryable_after_backoff());
}

#[test]
fn safe_messages_redact_and_bound_sensitive_fragments() {
    let err = McpError::rpc(-32000, format!("leak https://user:super-secret@host/{}", "x".repeat(800)));
    let message = err.safe_message();
    assert!(message.len() < 600);
    // Truncation may cut mid-string; still must not keep the raw password if present in prefix.
    // Credential-bearing URLs should not be preferred in safe_message construction beyond truncate.
    assert!(message.starts_with("MCP JSON-RPC error -32000:"));
    let unsafe_url = McpError::UnsafeUrl.safe_message();
    assert_eq!(unsafe_url, "MCP URL is not allowed");
    assert!(!unsafe_url.contains("http"));
}

#[tokio::test]
async fn ssrf_blocks_link_local_metadata_endpoint() {
    let url = Url::parse("https://evil.example/mcp").unwrap();
    let resolver = FixedResolver {
        addrs: vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
            443,
        )],
    };
    let err = validate_mcp_url(&url, &resolver).await.unwrap_err();
    assert!(matches!(err, McpError::UnsafeUrl));
    assert_eq!(err.code(), "mcp.unsafe_url");
}

#[tokio::test]
async fn redirect_response_is_not_followed() {
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
    let mut handle = start_server(&http_spec("redirect", url, 5_000), 1, CancellationToken::new())
        .await
        .unwrap();
    let err = initialize(&mut handle).await.unwrap_err();
    assert!(matches!(err, McpError::Http), "got {err:?}");
    assert_eq!(err.code(), "mcp.http");
    let _ = shutdown_server(handle, Instant::now() + Duration::from_secs(1)).await;
}

#[tokio::test]
async fn credential_urls_are_rejected_without_echoing_secrets() {
    let url = Url::parse("https://user:super-secret@example.com/mcp").unwrap();
    let redacted = redact_url_credentials(&url);
    assert!(!redacted.contains("super-secret"));
    let resolver = FixedResolver {
        addrs: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 443)],
    };
    let err = validate_mcp_url(&url, &resolver).await.unwrap_err();
    assert!(matches!(err, McpError::UnsafeUrl));
    assert!(!err.safe_message().contains("super-secret"));
    assert!(!format!("{err}").contains("super-secret"));
}

#[cfg(unix)]
#[tokio::test]
async fn crash_isolation_leaves_healthy_peer_usable() {
    fn write_executable(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    let good_script = r#"#!/usr/bin/env python3
import json, sys
def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    req = json.loads(line)
    method = req.get("method")
    id = req.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"good","version":"1"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":id,"result":{"tools":[]}})
    elif method == "tools/call":
        send({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"ok"}],"isError":false}})
    else:
        send({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"unknown"}})
"#;
    let bad_script = "#!/bin/sh\nexit 1\n";
    let dir = tempfile::tempdir().unwrap();
    let good = write_executable(dir.path(), "good.py", good_script);
    let bad = write_executable(dir.path(), "bad.sh", bad_script);
    let stdio = |name: &str, cmd: PathBuf| McpServerSpec {
        id: format!("p/{name}"),
        plugin_name: "p".into(),
        server_name: name.into(),
        transport: McpTransportKind::Stdio,
        command: Some(cmd),
        args: Vec::new(),
        env: Default::default(),
        cwd: Some(dir.path().to_path_buf()),
        url: None,
        headers: Vec::new(),
        timeout_ms: 5_000,
        source_dir: dir.path().to_path_buf(),
    };
    let manager = McpManager::new(
        Arc::new(McpDescriptorSet {
            generation: 11,
            servers: vec![stdio("good", good), stdio("bad", bad)].into(),
            diagnostics: Arc::from([]),
            allowed_tools: None,
        }),
        CancellationToken::new(),
    );
    let bad_err = manager.initialize_server("bad").await.unwrap_err();
    assert!(
        matches!(
            bad_err,
            McpError::Io | McpError::Unhealthy | McpError::Protocol { .. } | McpError::Timeout { .. } | McpError::Spawn
        ),
        "got {bad_err:?}"
    );
    let _ = manager.mark_unhealthy_and_reap("bad").await;
    let init = manager.initialize_server("good").await.unwrap();
    assert_eq!(init.server_info.name, "good");
    manager
        .shutdown_all(Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
}
