//! Tool discovery & schema cache fixtures (Phase 6C Task 3).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use lato_mcp::{McpDescriptorSet, McpDiagnostic, McpManager, McpServerSpec, McpTransportKind};

use tokio_util::sync::CancellationToken;

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

fn descriptors(generation: u64, servers: Vec<McpServerSpec>) -> Arc<McpDescriptorSet> {
    Arc::new(McpDescriptorSet {
        generation,
        servers: Arc::from(servers),
        diagnostics: Arc::from([]),
        allowed_tools: None,
    })
}

/// Stdio fixture with tools/list: good tools, a collision, and a bad schema.
fn discovery_stdio_script() -> &'static str {
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
                "serverInfo": {"name": "fixture-discovery", "version": "1.0.0"},
            },
        })
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        reply({
            "jsonrpc": "2.0",
            "id": msg["id"],
            "result": {
                "tools": [
                    {
                        "name": "echo",
                        "description": "first echo",
                        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
                    },
                    {
                        "name": "echo",
                        "description": "duplicate echo should lose",
                        "inputSchema": {"type": "object"},
                    },
                    {
                        "name": "bad_tool",
                        "description": "missing schema object",
                        "inputSchema": "nope",
                    },
                    {
                        "name": "list_items",
                        "description": "lists items",
                        "inputSchema": {"type": "object", "properties": {}},
                    },
                ]
            },
        })
    elif "id" in msg:
        reply({"jsonrpc": "2.0", "id": msg["id"], "result": {"ok": True}})
"#
}

/// Fixture that returns a different tools/list on every call (to prove cache stability).
fn mutating_tools_script() -> &'static str {
    r#"#!/usr/bin/env python3
import json, sys
state = {"n": 0}
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
                "serverInfo": {"name": "mutating", "version": "1.0.0"},
            },
        })
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        state["n"] += 1
        name = "tool_v%d" % state["n"]
        reply({
            "jsonrpc": "2.0",
            "id": msg["id"],
            "result": {
                "tools": [{
                    "name": name,
                    "description": "version %d" % state["n"],
                    "inputSchema": {"type": "object"},
                }]
            },
        })
    elif "id" in msg:
        reply({"jsonrpc": "2.0", "id": msg["id"], "result": {"ok": True}})
"#
}

fn diagnostic_codes(diags: &[McpDiagnostic]) -> Vec<&str> {
    diags.iter().map(|d| d.code.as_str()).collect()
}

#[cfg(unix)]
#[tokio::test]
async fn tools_list_cached_for_generation_and_lookup_works() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_executable(dir.path(), "server.py", discovery_stdio_script());
    let cancel = CancellationToken::new();
    let manager = McpManager::new(
        descriptors(
            42,
            vec![stdio_spec(
                "demo",
                PathBuf::from("python3"),
                vec![script.display().to_string()],
                5_000,
            )],
        ),
        cancel.clone(),
    );

    manager.ensure_discovered("demo").await.unwrap();
    let cache = manager.cache();
    assert_eq!(cache.generation(), 42);
    assert_eq!(cache.len(), 2, "echo + list_items retained");
    let echo = manager.lookup("demo__echo").expect("qualified lookup");
    assert_eq!(echo.description, "first echo");
    assert_eq!(echo.server, "demo");
    assert_eq!(echo.name, "echo");
    assert!(manager.lookup("demo/list_items").is_some());

    // Second discover must not re-hit tools/list (would still be stable even if it did
    // for this fixture; mutating fixture covers no re-fetch below).
    manager.ensure_discovered("demo").await.unwrap();
    assert_eq!(manager.cache().len(), 2);

    let _ = manager
        .shutdown_all(Instant::now() + Duration::from_secs(2))
        .await;
}

#[cfg(unix)]
#[tokio::test]
async fn collision_keeps_first_and_bad_schema_is_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_executable(dir.path(), "server.py", discovery_stdio_script());
    let manager = McpManager::new(
        descriptors(
            1,
            vec![stdio_spec(
                "demo",
                PathBuf::from("python3"),
                vec![script.display().to_string()],
                5_000,
            )],
        ),
        CancellationToken::new(),
    );
    manager.ensure_discovered("demo").await.unwrap();
    let cache = manager.cache();
    let codes = diagnostic_codes(cache.diagnostics());
    assert!(
        codes.contains(&"mcp.tool_collision"),
        "expected collision diagnostic, got {codes:?}"
    );
    assert!(
        codes.contains(&"mcp.tool_schema_invalid"),
        "expected bad schema diagnostic, got {codes:?}"
    );
    let echo = cache.lookup("demo__echo").unwrap();
    assert_eq!(echo.description, "first echo");
    assert!(cache.lookup("demo__bad_tool").is_none());
    assert!(cache.lookup("demo__list_items").is_some());
    let server_tools = cache.tools_for_server("demo").unwrap();
    assert_eq!(server_tools.len(), 2);
    let _ = manager
        .shutdown_all(Instant::now() + Duration::from_secs(2))
        .await;
}

#[cfg(unix)]
#[tokio::test]
async fn ensure_discovered_does_not_refetch_within_generation() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_executable(dir.path(), "mutating.py", mutating_tools_script());
    let manager = McpManager::new(
        descriptors(
            9,
            vec![stdio_spec(
                "mut",
                PathBuf::from("python3"),
                vec![script.display().to_string()],
                5_000,
            )],
        ),
        CancellationToken::new(),
    );
    manager.ensure_discovered("mut").await.unwrap();
    let first = manager.lookup("mut__tool_v1").expect("first list version");
    assert_eq!(first.description, "version 1");
    // If we re-fetched, tools/list would return tool_v2.
    manager.ensure_discovered("mut").await.unwrap();
    assert!(
        manager.lookup("mut__tool_v1").is_some(),
        "cache must stay on first discovery within generation"
    );
    assert!(
        manager.lookup("mut__tool_v2").is_none(),
        "must not adopt later tools/list within same generation"
    );
    assert_eq!(manager.cache().len(), 1);
    let _ = manager
        .shutdown_all(Instant::now() + Duration::from_secs(2))
        .await;
}
