//! Streamable HTTP client contract: Accept, session, SSE JSON-RPC (MCP 2025-03-26).

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use lato_mcp::{
    McpError, McpServerSpec, McpTransportKind, initialize, rpc, shutdown_server, start_server,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};
use tokio_util::sync::CancellationToken;
use url::Url;

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

fn initialize_result(id: &serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "serverInfo": {"name": "streamable-http", "version": "1.0.0"},
        }
    })
    .to_string()
}

fn parse_http_request(raw: &str) -> (String, String) {
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
    (head.to_string(), body.trim_end_matches('\0').to_string())
}

fn header_value(head: &str, name: &str) -> Option<String> {
    let needle = format!("{}:", name.to_ascii_lowercase());
    for line in head.lines().skip(1) {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix(&needle) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn accept_lists_json_and_sse(accept: &str) -> bool {
    let lower = accept.to_ascii_lowercase();
    lower.contains("application/json") && lower.contains("text/event-stream")
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut buf = vec![0u8; 16384];
    let n = socket.read(&mut buf).await.unwrap_or(0);
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

#[tokio::test]
async fn http_streamable_posts_require_json_and_sse_accept() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let seen_clone = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut socket, _)) = accepted else { break };
                    let seen = Arc::clone(&seen_clone);
                    tokio::spawn(async move {
                        let raw = read_http_request(&mut socket).await;
                        let (head, body) = parse_http_request(&raw);
                        if let Some(accept) = header_value(&head, "accept") {
                            seen.lock().unwrap().push(accept);
                        }
                        let value: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_else(|_| serde_json::json!({}));
                        let id = value.get("id").cloned().unwrap_or(serde_json::json!(1));
                        let payload = initialize_result(&id);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                            payload.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            }
        }
    });

    let url = Url::parse(&format!("http://{addr}/mcp")).unwrap();
    let spec = http_spec("accept", url, 5_000);
    let mut handle = start_server(&spec, 1, CancellationToken::new())
        .await
        .unwrap();
    initialize(&mut handle).await.unwrap();
    shutdown_server(handle, Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    let _ = shutdown_tx.send(());

    let accepts = seen.lock().unwrap().clone();
    assert!(
        accepts.iter().any(|value| accept_lists_json_and_sse(value)),
        "streamable HTTP POST must Accept application/json and text/event-stream, got {accepts:?}"
    );
}

#[tokio::test]
async fn http_initialize_session_id_is_echoed_on_later_posts() {
    let later_sessions: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let later_clone = Arc::clone(&later_sessions);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut socket, _)) = accepted else { break };
                    let later = Arc::clone(&later_clone);
                    tokio::spawn(async move {
                        let raw = read_http_request(&mut socket).await;
                        let (head, body) = parse_http_request(&raw);
                        let value: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_else(|_| serde_json::json!({}));
                        let method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
                        let id = value.get("id").cloned().unwrap_or(serde_json::json!(1));
                        let session = header_value(&head, "mcp-session-id");
                        let (status, extra_headers, payload) = if method == "initialize" {
                            (
                                200,
                                "Mcp-Session-Id: sess-abc\r\n".to_string(),
                                initialize_result(&id),
                            )
                        } else if method == "notifications/initialized" {
                            if session.as_deref() != Some("sess-abc") {
                                later.lock().unwrap().push(session.clone());
                                (
                                    400,
                                    String::new(),
                                    serde_json::json!({"error":"missing session"}).to_string(),
                                )
                            } else {
                                later.lock().unwrap().push(session.clone());
                                (202, String::new(), String::new())
                            }
                        } else if session.as_deref() != Some("sess-abc") {
                            later.lock().unwrap().push(session.clone());
                            (
                                400,
                                String::new(),
                                serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "error": {"code": -32000, "message": "missing session"}
                                })
                                .to_string(),
                            )
                        } else {
                            later.lock().unwrap().push(session.clone());
                            (
                                200,
                                String::new(),
                                serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {"tools": []}
                                })
                                .to_string(),
                            )
                        };
                        let response = if payload.is_empty() {
                            format!(
                                "HTTP/1.1 {status} Accepted\r\n{extra_headers}Content-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                        } else {
                            format!(
                                "HTTP/1.1 {status} OK\r\n{extra_headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                                payload.len()
                            )
                        };
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            }
        }
    });

    let url = Url::parse(&format!("http://{addr}/mcp")).unwrap();
    let spec = http_spec("session", url, 5_000);
    let mut handle = start_server(&spec, 1, CancellationToken::new())
        .await
        .unwrap();
    initialize(&mut handle)
        .await
        .expect("initialize with session");
    let listed = rpc(&mut handle, "tools/list", Some(serde_json::json!({})))
        .await
        .expect("tools/list must send Mcp-Session-Id");
    assert!(listed.get("tools").is_some(), "got {listed:?}");
    shutdown_server(handle, Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    let _ = shutdown_tx.send(());

    let sessions = later_sessions.lock().unwrap().clone();
    assert!(
        sessions
            .iter()
            .any(|value| value.as_deref() == Some("sess-abc")),
        "subsequent posts must echo Mcp-Session-Id, got {sessions:?}"
    );
}

#[tokio::test]
async fn http_reads_jsonrpc_result_from_open_sse_stream() {
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
                        let raw = read_http_request(&mut socket).await;
                        let (_, body) = parse_http_request(&raw);
                        let value: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_else(|_| serde_json::json!({}));
                        let id = value.get("id").cloned().unwrap_or(serde_json::json!(1));
                        let payload = initialize_result(&id);
                        let sse = format!("event: message\ndata: {payload}\n\n");
                        let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
                        let _ = socket.write_all(header.as_bytes()).await;
                        let _ = socket.write_all(sse.as_bytes()).await;
                        // Hold the stream open; the client must not wait for EOF.
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    });
                }
            }
        }
    });

    let url = Url::parse(&format!("http://{addr}/mcp")).unwrap();
    let spec = http_spec("sse", url, 5_000);
    let mut handle = start_server(&spec, 1, CancellationToken::new())
        .await
        .unwrap();
    let started = Instant::now();
    let init = initialize(&mut handle)
        .await
        .expect("SSE initialize result must be readable before the stream ends");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "SSE parse waited on stream end: {:?}",
        started.elapsed()
    );
    assert_eq!(init.server_info.name, "streamable-http");
    shutdown_server(handle, Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    let _ = shutdown_tx.send(());
}

#[derive(Clone, Debug)]
struct SeenHttp {
    method: String,
    session: Option<String>,
    accept: Option<String>,
    #[allow(dead_code)]
    last_event_id: Option<String>,
    #[allow(dead_code)]
    body: String,
}

fn http_method(head: &str) -> String {
    head.lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase()
}

fn json_ok(status: u16, extra_headers: &str, payload: &str) -> String {
    if payload.is_empty() {
        format!(
            "HTTP/1.1 {status} Accepted\r\n{extra_headers}Content-Length: 0\r\nConnection: close\r\n\r\n"
        )
    } else {
        format!(
            "HTTP/1.1 {status} OK\r\n{extra_headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        )
    }
}

fn handle_json_rpc(method: &str, session: Option<&str>, body: &str) -> (u16, String, String) {
    let value: serde_json::Value =
        serde_json::from_str(body).unwrap_or_else(|_| serde_json::json!({}));
    let id = value.get("id").cloned().unwrap_or(serde_json::json!(1));
    let rpc_method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
    if rpc_method == "initialize" {
        return (
            200,
            "Mcp-Session-Id: sess-abc\r\n".into(),
            initialize_result(&id),
        );
    }
    if rpc_method == "notifications/initialized" {
        return (202, String::new(), String::new());
    }
    if session != Some("sess-abc") {
        return (
            400,
            String::new(),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32000, "message": "missing session"}
            })
            .to_string(),
        );
    }
    if method == "POST" && value.get("result").is_some() && rpc_method.is_empty() {
        return (202, String::new(), String::new());
    }
    (
        200,
        String::new(),
        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}}).to_string(),
    )
}

#[tokio::test]
async fn http_shutdown_deletes_session() {
    let seen: Arc<Mutex<Vec<SeenHttp>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let seen_clone = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut socket, _)) = accepted else { break };
                    let seen = Arc::clone(&seen_clone);
                    tokio::spawn(async move {
                        let raw = read_http_request(&mut socket).await;
                        let (head, body) = parse_http_request(&raw);
                        let method = http_method(&head);
                        let session = header_value(&head, "mcp-session-id");
                        seen.lock().unwrap().push(SeenHttp {
                            method: method.clone(),
                            session: session.clone(),
                            accept: header_value(&head, "accept"),
                            last_event_id: header_value(&head, "last-event-id"),
                            body: body.clone(),
                        });
                        let response = if method == "DELETE" {
                            json_ok(200, "", "")
                        } else {
                            let (status, extra, payload) =
                                handle_json_rpc(&method, session.as_deref(), &body);
                            json_ok(status, &extra, &payload)
                        };
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            }
        }
    });

    let url = Url::parse(&format!("http://{addr}/mcp")).unwrap();
    let spec = http_spec("delete", url, 5_000);
    let mut handle = start_server(&spec, 1, CancellationToken::new())
        .await
        .unwrap();
    initialize(&mut handle).await.unwrap();
    shutdown_server(handle, Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = shutdown_tx.send(());

    let log = seen.lock().unwrap().clone();
    assert!(
        log.iter()
            .any(|row| row.method == "DELETE" && row.session.as_deref() == Some("sess-abc")),
        "shutdown must DELETE the MCP session, got {log:?}"
    );
}

#[tokio::test]
async fn http_opens_get_sse_after_initialize_and_tolerates_405() {
    let seen: Arc<Mutex<Vec<SeenHttp>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let seen_clone = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut socket, _)) = accepted else { break };
                    let seen = Arc::clone(&seen_clone);
                    tokio::spawn(async move {
                        let raw = read_http_request(&mut socket).await;
                        let (head, body) = parse_http_request(&raw);
                        let method = http_method(&head);
                        let session = header_value(&head, "mcp-session-id");
                        seen.lock().unwrap().push(SeenHttp {
                            method: method.clone(),
                            session: session.clone(),
                            accept: header_value(&head, "accept"),
                            last_event_id: header_value(&head, "last-event-id"),
                            body: body.clone(),
                        });
                        let response = if method == "GET" {
                            "HTTP/1.1 405 Method Not Allowed\r\nAllow: POST\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
                        } else if method == "DELETE" {
                            json_ok(200, "", "")
                        } else {
                            let (status, extra, payload) =
                                handle_json_rpc(&method, session.as_deref(), &body);
                            json_ok(status, &extra, &payload)
                        };
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            }
        }
    });

    let url = Url::parse(&format!("http://{addr}/mcp")).unwrap();
    let spec = http_spec("get405", url, 5_000);
    let mut handle = start_server(&spec, 1, CancellationToken::new())
        .await
        .unwrap();
    initialize(&mut handle)
        .await
        .expect("GET 405 must not fail initialize");
    let listed = rpc(&mut handle, "tools/list", Some(serde_json::json!({})))
        .await
        .expect("POST still works after GET 405");
    assert!(listed.get("tools").is_some());
    shutdown_server(handle, Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let _ = shutdown_tx.send(());

    let log = seen.lock().unwrap().clone();
    assert!(
        log.iter().any(|row| {
            row.method == "GET"
                && row.session.as_deref() == Some("sess-abc")
                && row
                    .accept
                    .as_deref()
                    .is_some_and(|accept| accept.to_ascii_lowercase().contains("text/event-stream"))
        }),
        "client must GET SSE after initialize, got {log:?}"
    );
}

#[tokio::test]
async fn http_answers_ping_from_get_sse() {
    let pongs: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let pongs_clone = Arc::clone(&pongs);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut socket, _)) = accepted else { break };
                    let pongs = Arc::clone(&pongs_clone);
                    tokio::spawn(async move {
                        let raw = read_http_request(&mut socket).await;
                        let (head, body) = parse_http_request(&raw);
                        let method = http_method(&head);
                        let session = header_value(&head, "mcp-session-id");
                        if method == "GET" {
                            let ping = serde_json::json!({"jsonrpc":"2.0","id":99,"method":"ping"});
                            let sse = format!("event: message\ndata: {ping}\n\n");
                            let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
                            let _ = socket.write_all(header.as_bytes()).await;
                            let _ = socket.write_all(sse.as_bytes()).await;
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            return;
                        }
                        let value: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_else(|_| serde_json::json!({}));
                        if value.get("result").is_some() && value.get("method").is_none() {
                            pongs.lock().unwrap().push(value);
                            let response = json_ok(202, "", "");
                            let _ = socket.write_all(response.as_bytes()).await;
                            return;
                        }
                        let (status, extra, payload) =
                            handle_json_rpc(&method, session.as_deref(), &body);
                        let response = if method == "DELETE" {
                            json_ok(200, "", "")
                        } else {
                            json_ok(status, &extra, &payload)
                        };
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            }
        }
    });

    let url = Url::parse(&format!("http://{addr}/mcp")).unwrap();
    let spec = http_spec("ping", url, 5_000);
    let mut handle = start_server(&spec, 1, CancellationToken::new())
        .await
        .unwrap();
    initialize(&mut handle).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if !pongs.lock().unwrap().is_empty() || Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    shutdown_server(handle, Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    let _ = shutdown_tx.send(());

    let pongs = pongs.lock().unwrap().clone();
    assert!(
        pongs
            .iter()
            .any(|value| value.get("id") == Some(&serde_json::json!(99))
                && value.get("result").is_some()),
        "GET ping must be answered with a JSON-RPC result, got {pongs:?}"
    );
}

#[tokio::test]
async fn http_session_404_marks_server_unhealthy() {
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
                        let raw = read_http_request(&mut socket).await;
                        let (head, body) = parse_http_request(&raw);
                        let method = http_method(&head);
                        let value: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_else(|_| serde_json::json!({}));
                        let rpc_method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
                        let response = if method == "GET" {
                            "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
                        } else if method == "DELETE" {
                            json_ok(200, "", "")
                        } else if rpc_method == "initialize" {
                            let id = value.get("id").cloned().unwrap_or(serde_json::json!(1));
                            json_ok(200, "Mcp-Session-Id: sess-abc\r\n", &initialize_result(&id))
                        } else if rpc_method == "notifications/initialized" {
                            json_ok(202, "", "")
                        } else {
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
                        };
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            }
        }
    });

    let url = Url::parse(&format!("http://{addr}/mcp")).unwrap();
    let spec = http_spec("gone", url, 5_000);
    let mut handle = start_server(&spec, 1, CancellationToken::new())
        .await
        .unwrap();
    initialize(&mut handle).await.unwrap();
    let err = rpc(&mut handle, "tools/list", Some(serde_json::json!({})))
        .await
        .unwrap_err();
    assert!(
        matches!(err, McpError::Unhealthy),
        "expired session 404 must map to unhealthy, got {err:?}"
    );
    shutdown_server(handle, Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn http_session_404_reinitializes_without_session_and_retries() {
    let inits: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let inits_clone = Arc::clone(&inits);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut socket, _)) = accepted else { break };
                    let inits = Arc::clone(&inits_clone);
                    tokio::spawn(async move {
                        let raw = read_http_request(&mut socket).await;
                        let (head, body) = parse_http_request(&raw);
                        let method = http_method(&head);
                        let session = header_value(&head, "mcp-session-id");
                        let value: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_else(|_| serde_json::json!({}));
                        let rpc_method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
                        let id = value.get("id").cloned().unwrap_or(serde_json::json!(1));
                        let response = if method == "GET" {
                            "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
                        } else if method == "DELETE" {
                            json_ok(200, "", "")
                        } else if rpc_method == "initialize" {
                            inits.lock().unwrap().push(session.clone());
                            let minted = if inits.lock().unwrap().len() == 1 {
                                "sess-old"
                            } else {
                                "sess-new"
                            };
                            json_ok(
                                200,
                                &format!("Mcp-Session-Id: {minted}\r\n"),
                                &initialize_result(&id),
                            )
                        } else if rpc_method == "notifications/initialized" {
                            json_ok(202, "", "")
                        } else if session.as_deref() == Some("sess-old") {
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
                        } else if session.as_deref() == Some("sess-new") {
                            json_ok(
                                200,
                                "",
                                &serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {"tools": [{"name": "echo"}]}
                                })
                                .to_string(),
                            )
                        } else {
                            json_ok(400, "", "")
                        };
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            }
        }
    });

    let url = Url::parse(&format!("http://{addr}/mcp")).unwrap();
    let spec = http_spec("reinit", url, 5_000);
    let mut handle = start_server(&spec, 1, CancellationToken::new())
        .await
        .unwrap();
    initialize(&mut handle).await.unwrap();
    let listed = rpc(&mut handle, "tools/list", Some(serde_json::json!({})))
        .await
        .expect("404 session must re-initialize and retry");
    assert_eq!(listed["tools"][0]["name"], "echo");
    shutdown_server(handle, Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    let _ = shutdown_tx.send(());

    let inits = inits.lock().unwrap().clone();
    assert_eq!(
        inits.len(),
        2,
        "expected a second initialize, got {inits:?}"
    );
    assert_eq!(inits[0], None, "first initialize has no session yet");
    assert_eq!(
        inits[1], None,
        "re-initialize MUST omit Mcp-Session-Id, got {inits:?}"
    );
}

#[tokio::test]
async fn http_get_reconnect_sends_last_event_id() {
    let gets: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let gets_clone = Arc::clone(&gets);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut socket, _)) = accepted else { break };
                    let gets = Arc::clone(&gets_clone);
                    tokio::spawn(async move {
                        let raw = read_http_request(&mut socket).await;
                        let (head, body) = parse_http_request(&raw);
                        let method = http_method(&head);
                        let session = header_value(&head, "mcp-session-id");
                        if method == "GET" {
                            let last = header_value(&head, "last-event-id");
                            let n = {
                                let mut log = gets.lock().unwrap();
                                log.push(last.clone());
                                log.len()
                            };
                            if n == 1 {
                                let ping = serde_json::json!({"jsonrpc":"2.0","id":7,"method":"ping"});
                                let sse = format!("id: evt-1\nevent: message\ndata: {ping}\n\n");
                                let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
                                let _ = socket.write_all(header.as_bytes()).await;
                                let _ = socket.write_all(sse.as_bytes()).await;
                            } else {
                                let _ = socket.write_all(
                                    b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                ).await;
                            }
                            return;
                        }
                        let (status, extra, payload) =
                            handle_json_rpc(&method, session.as_deref(), &body);
                        let response = if method == "DELETE" {
                            json_ok(200, "", "")
                        } else {
                            json_ok(status, &extra, &payload)
                        };
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            }
        }
    });

    let url = Url::parse(&format!("http://{addr}/mcp")).unwrap();
    let spec = http_spec("resume", url, 5_000);
    let mut handle = start_server(&spec, 1, CancellationToken::new())
        .await
        .unwrap();
    initialize(&mut handle).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let count = gets.lock().unwrap().len();
        if count >= 2 || Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    shutdown_server(handle, Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    let _ = shutdown_tx.send(());

    let gets = gets.lock().unwrap().clone();
    assert!(
        gets.len() >= 2,
        "GET stream should reconnect after EOF, got {gets:?}"
    );
    assert_eq!(gets[0], None, "first GET has no Last-Event-ID");
    assert_eq!(
        gets[1].as_deref(),
        Some("evt-1"),
        "reconnect GET must send Last-Event-ID, got {gets:?}"
    );
}
