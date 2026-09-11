// Derived from: Lato hooks HTTPS runner SSRF + redirect::Policy::none
// (crates/lato-extensions/src/hooks/http.rs).
// License: Apache-2.0 (workspace)
// Lato changes: streamable HTTP MCP JSON-RPC client; loopback-only plain http;
// redacts URL credentials in user-visible errors; no ambient auth beyond spec headers.

//! Streamable HTTP MCP transport with SSRF checks and no redirects.

use std::{
    io,
    net::{IpAddr, SocketAddr},
    time::Duration,
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
    protocol::{self, parse_response_value},
};

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
pub async fn validate_mcp_url(
    url: &Url,
    resolver: &dyn McpDnsResolver,
) -> Result<(), McpError> {
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
    closed: std::sync::atomic::AtomicBool,
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
            cancel,
            closed: std::sync::atomic::AtomicBool::new(false),
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
        if self
            .closed
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(McpError::ShutDown);
        }
        // Re-validate before each call so DNS rebinding cannot widen the grant.
        validate_mcp_url(&self.url, resolver).await?;
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let body = protocol::request(id, method, params);
        let timeout_ms = timeout.as_millis() as u64;
        let operation = async {
            let mut request = self
                .client
                .post(self.url.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(&body);
            for (name, value) in &self.headers {
                request = request.header(name, value);
            }
            let response = request.send().await.map_err(|error| map_http_error(error))?;
            let status = response.status();
            if status.is_redirection() {
                return Err(McpError::Http);
            }
            if !status.is_success() {
                return Err(McpError::Http);
            }
            let value: Value = response.json().await.map_err(|_| McpError::Http)?;
            parse_response_value(value)
        };
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
        let _ = self
            .request_notification(method, params, timeout, resolver)
            .await?;
        Ok(())
    }

    async fn request_notification(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
        resolver: &dyn McpDnsResolver,
    ) -> Result<(), McpError> {
        if self
            .closed
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(McpError::ShutDown);
        }
        validate_mcp_url(&self.url, resolver).await?;
        let body = protocol::notification(method, params);
        let timeout_ms = timeout.as_millis() as u64;
        let operation = async {
            let mut request = self
                .client
                .post(self.url.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(&body);
            for (name, value) in &self.headers {
                request = request.header(name, value);
            }
            let response = request.send().await.map_err(|error| map_http_error(error))?;
            let status = response.status();
            if status.is_redirection() || (!status.is_success() && status.as_u16() != 202) {
                return Err(McpError::Http);
            }
            Ok(())
        };
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(McpError::Cancelled),
            _ = tokio::time::sleep(timeout) => Err(McpError::Timeout { timeout_ms }),
            result = operation => result,
        }
    }

    pub fn shutdown(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn map_http_error(error: reqwest::Error) -> McpError {
    // reqwest may include the URL; never surface credential-bearing forms.
    let _ = error;
    McpError::Http
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn redacts_userinfo() {
        let url = Url::parse("https://user:secret@example.com/mcp").unwrap();
        let redacted = redact_url_credentials(&url);
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("user:"));
        assert!(redacted.contains("example.com/mcp"));
    }
}
