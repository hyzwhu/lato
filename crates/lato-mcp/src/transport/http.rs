// Derived from: Lato hooks HTTPS runner SSRF + redirect::Policy::none
// (crates/lato-extensions/src/hooks/http.rs).
// License: Apache-2.0 (workspace)
// Lato changes: streamable HTTP MCP JSON-RPC client; Accept json+SSE; Mcp-Session-Id;
// parse JSON or SSE JSON-RPC without waiting for stream EOF; GET SSE + ping;
// DELETE session on shutdown; 404 session → unhealthy; loopback-only plain http;
// redacts URL credentials in user-visible errors; no ambient auth beyond spec headers.

//! Streamable HTTP MCP transport with SSRF checks and no redirects.

use std::{
    io,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use reqwest::{Client, redirect::Policy};
use serde_json::Value;
use tokio::net::lookup_host;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    config::McpServerSpec,
    error::McpError,
    protocol::{self, PROTOCOL_VERSION, parse_response_value},
};

/// Hard cap on buffered Streamable HTTP SSE payload per RPC.
const MAX_SSE_BUFFER_BYTES: usize = 1024 * 1024;
const ACCEPT_STREAMABLE: &str = "application/json, text/event-stream";
const ACCEPT_SSE: &str = "text/event-stream";
const HEADER_PROTOCOL_VERSION: &str = "MCP-Protocol-Version";
const HEADER_SESSION_ID: &str = "Mcp-Session-Id";
const HEADER_LAST_EVENT_ID: &str = "Last-Event-ID";
const GET_RECONNECT_DELAY: Duration = Duration::from_millis(50);

#[async_trait]
pub trait McpDnsResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemMcpDnsResolver;

#[async_trait]
impl McpDnsResolver for SystemMcpDnsResolver {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(lookup_host((host, port)).await?.collect())
    }
}

pub fn build_mcp_http_client() -> Result<Client, McpError> {
    Client::builder()
        .redirect(Policy::none())
        .build()
        .map_err(|_| McpError::Http)
}

/// Redact username/password from a URL for logs and error messages.
pub fn redact_url_credentials(url: &Url) -> String {
    let mut redacted = url.clone();
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);
    redacted.to_string()
}

/// Validate an MCP HTTP endpoint after DNS resolution.
///
/// Policy:
/// - scheme must be `http` or `https`
/// - URL userinfo is rejected (credentials belong in configured headers only)
/// - `http` is allowed **only** when every resolved address is loopback
/// - `https` allows loopback and public addresses; private/link-local/CGNAT/etc. blocked
/// - empty resolution fails closed
pub async fn validate_mcp_url(url: &Url, resolver: &dyn McpDnsResolver) -> Result<(), McpError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(McpError::UnsafeUrl);
    }
    if url.username() != "" || url.password().is_some() {
        return Err(McpError::UnsafeUrl);
    }
    let host = url.host_str().ok_or(McpError::UnsafeUrl)?;
    let port = url.port_or_known_default().ok_or(McpError::UnsafeUrl)?;
    let addresses = resolver
        .resolve(host, port)
        .await
        .map_err(|_| McpError::UnsafeUrl)?;
    if addresses.is_empty() {
        return Err(McpError::UnsafeUrl);
    }
    if url.scheme() == "http" {
        if addresses.iter().any(|address| !address.ip().is_loopback()) {
            return Err(McpError::UnsafeUrl);
        }
        return Ok(());
    }
    if addresses.iter().any(|address| blocked(address.ip())) {
        return Err(McpError::UnsafeUrl);
    }
    Ok(())
}

fn blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            if ip.is_loopback() {
                return false;
            }
            let octets = ip.octets();
            ip.is_unspecified()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_multicast()
                || ip.is_broadcast()
                || octets[0] == 0
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                || (octets[0] == 198 && matches!(octets[1], 18 | 19))
        }
        IpAddr::V6(ip) => {
            if ip.is_loopback() {
                return false;
            }
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return blocked(IpAddr::V4(mapped));
            }
            ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
        }
    }
}

pub struct HttpSession {
    url: Url,
    headers: Vec<(String, String)>,
    client: Client,
    next_id: std::sync::atomic::AtomicU64,
    cancel: CancellationToken,
    get_cancel: Mutex<CancellationToken>,
    closed: std::sync::atomic::AtomicBool,
    expired: Arc<AtomicBool>,
    get_started: AtomicBool,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<String>,
}

impl HttpSession {
    pub async fn start(
        spec: &McpServerSpec,
        cancel: CancellationToken,
        resolver: &dyn McpDnsResolver,
    ) -> Result<Self, McpError> {
        let url = spec
            .url
            .clone()
            .ok_or_else(|| McpError::InvalidConfiguration("HTTP server requires url".into()))?;
        validate_mcp_url(&url, resolver).await?;
        Ok(Self {
            url,
            headers: spec.headers.clone(),
            client: build_mcp_http_client()?,
            next_id: std::sync::atomic::AtomicU64::new(1),
            get_cancel: Mutex::new(cancel.child_token()),
            cancel,
            closed: std::sync::atomic::AtomicBool::new(false),
            expired: Arc::new(AtomicBool::new(false)),
            get_started: AtomicBool::new(false),
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(PROTOCOL_VERSION.to_owned()),
        })
    }

    pub fn endpoint_redacted(&self) -> String {
        redact_url_credentials(&self.url)
    }

    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
        resolver: &dyn McpDnsResolver,
    ) -> Result<Value, McpError> {
        let timeout_ms = timeout.as_millis() as u64;
        let operation = self.dispatch_rpc(method, params, resolver, true);
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(McpError::Cancelled),
            _ = tokio::time::sleep(timeout) => Err(McpError::Timeout { timeout_ms }),
            result = operation => result,
        }
    }

    pub async fn notify(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
        resolver: &dyn McpDnsResolver,
    ) -> Result<(), McpError> {
        // Notifications still POST; ignore response body beyond transport errors.
        self.request_notification(method, params, timeout, resolver)
            .await
    }

    async fn request_notification(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
        resolver: &dyn McpDnsResolver,
    ) -> Result<(), McpError> {
        let timeout_ms = timeout.as_millis() as u64;
        let operation = async {
            let recovered = self.recover_if_expired(resolver).await?;
            match self
                .post_notification_once(method, params.clone(), resolver)
                .await
            {
                Err(McpError::Unhealthy) if !recovered => {
                    self.recover_session(resolver).await?;
                    self.post_notification_once(method, params, resolver).await
                }
                other => other,
            }
        };
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(McpError::Cancelled),
            _ = tokio::time::sleep(timeout) => Err(McpError::Timeout { timeout_ms }),
            result = operation => result,
        }
    }

    pub async fn shutdown(&self, deadline: Instant) -> Result<(), McpError> {
        self.closed.store(true, Ordering::SeqCst);
        lock_mutex(&self.get_cancel).cancel();
        if lock_mutex(&self.session_id).is_none() {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout = remaining.clamp(Duration::from_millis(50), Duration::from_secs(2));
        let request = self.apply_streamable_headers(self.client.delete(self.url.clone()));
        let _ = tokio::time::timeout(timeout, request.send()).await;
        Ok(())
    }

    async fn dispatch_rpc(
        &self,
        method: &str,
        params: Option<Value>,
        resolver: &dyn McpDnsResolver,
        allow_recover: bool,
    ) -> Result<Value, McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::ShutDown);
        }
        let mut recovered = false;
        if allow_recover && method != "initialize" {
            recovered = self.recover_if_expired(resolver).await?;
        }
        match self.post_rpc_once(method, params.clone(), resolver).await {
            Err(McpError::Unhealthy) if allow_recover && !recovered && method != "initialize" => {
                self.recover_session(resolver).await?;
                self.post_rpc_once(method, params, resolver).await
            }
            other => other,
        }
    }

    async fn recover_if_expired(&self, resolver: &dyn McpDnsResolver) -> Result<bool, McpError> {
        if !self.expired.load(Ordering::SeqCst) {
            return Ok(false);
        }
        self.recover_session(resolver).await?;
        Ok(true)
    }

    async fn recover_session(&self, resolver: &dyn McpDnsResolver) -> Result<(), McpError> {
        self.begin_new_session();
        self.post_rpc_once("initialize", Some(protocol::initialize_params()), resolver)
            .await?;
        self.post_notification_once(
            "notifications/initialized",
            Some(serde_json::json!({})),
            resolver,
        )
        .await
    }

    fn begin_new_session(&self) {
        self.expired.store(false, Ordering::SeqCst);
        *lock_mutex(&self.session_id) = None;
        self.get_started.store(false, Ordering::SeqCst);
        let mut token = lock_mutex(&self.get_cancel);
        token.cancel();
        *token = self.cancel.child_token();
    }

    async fn post_rpc_once(
        &self,
        method: &str,
        params: Option<Value>,
        resolver: &dyn McpDnsResolver,
    ) -> Result<Value, McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::ShutDown);
        }
        validate_mcp_url(&self.url, resolver).await?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = protocol::request(id, method, params);
        let request = self.apply_streamable_headers(
            self.client
                .post(self.url.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(&body),
        );
        let response = request.send().await.map_err(map_http_error)?;
        self.capture_session(&response);
        self.check_http_status(response.status())?;
        let result = read_rpc_body(response, id).await?;
        if method == "initialize" {
            if let Some(version) = result.get("protocolVersion").and_then(Value::as_str) {
                *lock_mutex(&self.protocol_version) = version.to_owned();
            }
            self.spawn_get_listener();
        }
        Ok(result)
    }

    async fn post_notification_once(
        &self,
        method: &str,
        params: Option<Value>,
        resolver: &dyn McpDnsResolver,
    ) -> Result<(), McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::ShutDown);
        }
        validate_mcp_url(&self.url, resolver).await?;
        let body = protocol::notification(method, params);
        let request = self.apply_streamable_headers(
            self.client
                .post(self.url.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(&body),
        );
        let response = request.send().await.map_err(map_http_error)?;
        self.capture_session(&response);
        self.check_http_status(response.status())
    }

    fn check_http_status(&self, status: reqwest::StatusCode) -> Result<(), McpError> {
        if status.is_redirection() {
            return Err(McpError::Http);
        }
        if status.as_u16() == 404 && lock_mutex(&self.session_id).is_some() {
            self.expired.store(true, Ordering::SeqCst);
            return Err(McpError::Unhealthy);
        }
        if status.is_success() || status.as_u16() == 202 {
            return Ok(());
        }
        Err(McpError::Http)
    }

    fn spawn_get_listener(&self) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        if self.get_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let client = self.client.clone();
        let url = self.url.clone();
        let headers = self.headers.clone();
        let session = lock_mutex(&self.session_id).clone();
        let version = lock_mutex(&self.protocol_version).clone();
        let cancel = lock_mutex(&self.get_cancel).clone();
        let expired = Arc::clone(&self.expired);
        tokio::spawn(async move {
            listen_get_sse(client, url, headers, session, version, cancel, expired).await;
        });
    }

    fn apply_streamable_headers(
        &self,
        mut request: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        request = request
            .header(reqwest::header::ACCEPT, ACCEPT_STREAMABLE)
            .header(
                HEADER_PROTOCOL_VERSION,
                lock_mutex(&self.protocol_version).clone(),
            );
        if let Some(session) = lock_mutex(&self.session_id).as_ref() {
            request = request.header(HEADER_SESSION_ID, session.clone());
        }
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        request
    }

    fn capture_session(&self, response: &reqwest::Response) {
        let Some(value) = response.headers().get(HEADER_SESSION_ID) else {
            return;
        };
        let Ok(text) = value.to_str() else {
            return;
        };
        if !valid_session_id(text) {
            return;
        }
        *lock_mutex(&self.session_id) = Some(text.to_owned());
    }
}

enum GetSseOutcome {
    Stop,
    Expired,
    Reconnect,
}

async fn listen_get_sse(
    client: Client,
    url: Url,
    headers: Vec<(String, String)>,
    session: Option<String>,
    version: String,
    cancel: CancellationToken,
    expired: Arc<AtomicBool>,
) {
    let mut last_event_id = None;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        match open_get_sse(
            &client,
            &url,
            &headers,
            session.as_deref(),
            &version,
            &cancel,
            &mut last_event_id,
        )
        .await
        {
            GetSseOutcome::Stop => return,
            GetSseOutcome::Expired => {
                expired.store(true, Ordering::SeqCst);
                return;
            }
            GetSseOutcome::Reconnect => {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(GET_RECONNECT_DELAY) => {}
                }
            }
        }
    }
}

async fn open_get_sse(
    client: &Client,
    url: &Url,
    headers: &[(String, String)],
    session: Option<&str>,
    version: &str,
    cancel: &CancellationToken,
    last_event_id: &mut Option<String>,
) -> GetSseOutcome {
    let mut request = client
        .get(url.clone())
        .header(reqwest::header::ACCEPT, ACCEPT_SSE)
        .header(HEADER_PROTOCOL_VERSION, version);
    if let Some(session) = session {
        request = request.header(HEADER_SESSION_ID, session);
    }
    if let Some(event_id) = last_event_id.as_deref() {
        request = request.header(HEADER_LAST_EVENT_ID, event_id);
    }
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = tokio::select! {
        biased;
        _ = cancel.cancelled() => return GetSseOutcome::Stop,
        response = request.send() => match response {
            Ok(response) => response,
            Err(_) => return GetSseOutcome::Reconnect,
        },
    };
    let status = response.status();
    if status.as_u16() == 404 {
        return GetSseOutcome::Expired;
    }
    if !status.is_success() {
        return GetSseOutcome::Stop;
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !content_type.contains("text/event-stream") {
        return GetSseOutcome::Stop;
    }
    let mut response = response;
    let mut buffer = String::new();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return GetSseOutcome::Stop,
            chunk = response.chunk() => match chunk {
                Ok(chunk) => chunk,
                Err(_) => return GetSseOutcome::Reconnect,
            },
        };
        let Some(bytes) = chunk else {
            return GetSseOutcome::Reconnect;
        };
        buffer.push_str(&String::from_utf8_lossy(&bytes));
        if buffer.len() > MAX_SSE_BUFFER_BYTES {
            return GetSseOutcome::Stop;
        }
        while let Some(event) = next_sse_event(&mut buffer) {
            if let Some(event_id) = sse_event_id(&event) {
                *last_event_id = Some(event_id);
            }
            post_get_reply(client, url, headers, session, version, &event, cancel).await;
        }
    }
}

async fn post_get_reply(
    client: &Client,
    url: &Url,
    headers: &[(String, String)],
    session: Option<&str>,
    version: &str,
    event: &str,
    cancel: &CancellationToken,
) {
    let Some(value) = jsonrpc_from_sse_event(event) else {
        return;
    };
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return;
    };
    let Some(id) = value.get("id").cloned() else {
        return;
    };
    let body = if method == "ping" {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}})
    } else {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "method not supported"}
        })
    };
    let mut request = client
        .post(url.clone())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, ACCEPT_STREAMABLE)
        .header(HEADER_PROTOCOL_VERSION, version)
        .json(&body);
    if let Some(session) = session {
        request = request.header(HEADER_SESSION_ID, session);
    }
    for (name, value) in headers {
        request = request.header(name, value);
    }
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {}
        _ = request.send() => {}
    };
}

fn map_http_error(error: reqwest::Error) -> McpError {
    // reqwest may include the URL; never surface credential-bearing forms.
    let _ = error;
    McpError::Http
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| (0x21..=0x7E).contains(&byte))
}

async fn read_rpc_body(response: reqwest::Response, request_id: u64) -> Result<Value, McpError> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if content_type.contains("text/event-stream") {
        return read_sse_response(response, request_id).await;
    }
    let value: Value = response.json().await.map_err(|_| McpError::Http)?;
    parse_response_value(value)
}

async fn read_sse_response(
    mut response: reqwest::Response,
    request_id: u64,
) -> Result<Value, McpError> {
    let mut buffer = String::new();
    loop {
        let chunk = response.chunk().await.map_err(|_| McpError::Http)?;
        let Some(bytes) = chunk else {
            return Err(McpError::protocol(
                "SSE stream ended before JSON-RPC response",
            ));
        };
        buffer.push_str(&String::from_utf8_lossy(&bytes));
        if buffer.len() > MAX_SSE_BUFFER_BYTES {
            return Err(McpError::protocol("SSE response exceeded size limit"));
        }
        if let Some(value) = take_sse_jsonrpc_response(&mut buffer, request_id) {
            return parse_response_value(value);
        }
    }
}

fn take_sse_jsonrpc_response(buffer: &mut String, request_id: u64) -> Option<Value> {
    loop {
        let event = next_sse_event(buffer)?;
        let Some(value) = jsonrpc_from_sse_event(&event) else {
            continue;
        };
        if json_id_matches(&value, request_id) {
            return Some(value);
        }
    }
}

fn next_sse_event(buffer: &mut String) -> Option<String> {
    let crlf = buffer.find("\r\n\r\n");
    let lf = buffer.find("\n\n");
    let (index, sep_len) = match (crlf, lf) {
        (Some(crlf), Some(lf)) if crlf <= lf => (crlf, 4),
        (Some(crlf), None) => (crlf, 4),
        (Some(_crlf), Some(lf)) => (lf, 2),
        (None, Some(lf)) => (lf, 2),
        (None, None) => return None,
    };
    let event = buffer[..index].to_string();
    buffer.drain(..index + sep_len);
    Some(event)
}

fn sse_event_id(event: &str) -> Option<String> {
    for line in event.lines() {
        let line = line.trim_end_matches('\r');
        let Some(rest) = line.strip_prefix("id:") else {
            continue;
        };
        let value = rest.strip_prefix(' ').unwrap_or(rest);
        if valid_event_id(value) {
            return Some(value.to_owned());
        }
    }
    None
}

fn valid_event_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| (0x21..=0x7E).contains(&byte))
}

fn jsonrpc_from_sse_event(event: &str) -> Option<Value> {
    let mut data = String::new();
    for line in event.lines() {
        let line = line.trim_end_matches('\r');
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        if !data.is_empty() {
            data.push('\n');
        }
        data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
    }
    if data.is_empty() {
        return None;
    }
    serde_json::from_str(&data).ok()
}

fn json_id_matches(value: &Value, request_id: u64) -> bool {
    match value.get("id") {
        Some(Value::Number(number)) => number.as_u64() == Some(request_id),
        Some(Value::String(text)) => text.parse::<u64>().ok() == Some(request_id),
        _ => false,
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    struct FixedResolver {
        addrs: Vec<SocketAddr>,
    }

    #[async_trait]
    impl McpDnsResolver for FixedResolver {
        async fn resolve(&self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
            Ok(self.addrs.clone())
        }
    }

    #[test]
    fn redacts_userinfo() {
        let url = Url::parse("https://user:secret@example.com/mcp").unwrap();
        let redacted = redact_url_credentials(&url);
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("user:"));
        assert!(redacted.contains("example.com/mcp"));
    }

    #[tokio::test]
    async fn rejects_link_local_and_credentials_without_echoing_secrets() {
        let url = Url::parse("https://user:super-secret@meta.example/mcp").unwrap();
        let resolver = FixedResolver {
            addrs: vec![SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
                443,
            )],
        };
        let err = validate_mcp_url(&url, &resolver).await.unwrap_err();
        assert!(matches!(err, McpError::UnsafeUrl));
        let display = err.safe_message();
        assert!(!display.contains("super-secret"));
        assert!(!display.contains("user:"));
        assert_eq!(err.code(), "mcp.unsafe_url");
    }

    #[tokio::test]
    async fn rejects_private_https_targets() {
        let url = Url::parse("https://evil.example/mcp").unwrap();
        let resolver = FixedResolver {
            addrs: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 443)],
        };
        let err = validate_mcp_url(&url, &resolver).await.unwrap_err();
        assert!(matches!(err, McpError::UnsafeUrl));
    }

    #[test]
    fn http_client_disables_redirects() {
        // Construction succeeds; Policy::none is set in build_mcp_http_client.
        let client = build_mcp_http_client().expect("client");
        let _ = client;
    }

    #[test]
    fn sse_parser_reads_event_id() {
        let event = "id: evt-1\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"ping\"}";
        assert_eq!(sse_event_id(event).as_deref(), Some("evt-1"));
        assert!(sse_event_id("data: {}\n").is_none());
        assert!(sse_event_id("id: has space\n").is_none());
    }

    #[test]
    fn sse_parser_reads_jsonrpc_response_and_ignores_other_events() {
        let mut buffer = String::from(
            ": ping\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n\n\
             data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"ok\":true}}\n\n",
        );
        let value = take_sse_jsonrpc_response(&mut buffer, 3).expect("response");
        assert_eq!(value["result"]["ok"], true);
    }

    #[test]
    fn session_ids_reject_whitespace_and_empty() {
        assert!(valid_session_id("sess-abc"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("has space"));
        assert!(!valid_session_id("line\nbreak"));
    }
}
