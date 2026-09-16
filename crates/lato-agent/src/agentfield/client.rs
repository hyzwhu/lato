// Phase 7C1: `AgentFieldClient` trait, strict HTTP adapter, and transport
// seam. The transport abstraction keeps the contract tests offline
// (`no-live-network`): unit tests drive a fake transport, while the reqwest
// adapter is exercised in doctor/live flows only.

use crate::agentfield::config::{ControlPlaneOrigin, PINNED_AGENTFIELD_VERSION};
use crate::agentfield::types::{
    AsyncStartEnvelope, CancelConflictEnvelope, CancelSuccessEnvelope, DiscoveryEnvelope,
    StatusEnvelope,
};
use serde_json::Value;
use std::{net::SocketAddr, sync::Arc};

/// Maximum response body bytes Lato accepts (compression bombs and runaway
/// bodies are rejected before parsing).
pub const MAX_RESPONSE_BODY_BYTES: usize = 1024 * 1024;

/// Stable error family for every client failure (spec §11).
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AgentFieldError {
    /// DNS/TLS/connect/timeout/5xx/redirect violations: transient.
    #[error("agentfield.unavailable: {0}")]
    Unavailable(String),
    /// 401/403: credential problem; server body is never echoed.
    #[error("agentfield.unauthorized: credentials rejected")]
    Unauthorized,
    /// Version, envelope, or status violations.
    #[error("agentfield.remote_protocol: {0}")]
    RemoteProtocol(String),
    /// Remote policy explicitly denied the call.
    #[error("agentfield.remote_denied")]
    RemoteDenied,
    /// Response body exceeded the transport cap.
    #[error("agentfield.output_too_large: response body exceeds {0} bytes")]
    BodyTooLarge(usize),
}

/// Redacted Bearer token wrapper: the secret is never printable.
#[derive(Clone)]
pub struct RedactedToken(String);

impl RedactedToken {
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// Expose the secret only for final header construction.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for RedactedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[redacted]")
    }
}

impl std::fmt::Display for RedactedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[redacted]")
    }
}

/// Minimal HTTP surface the adapter needs; keeps fake-server tests offline.
#[derive(Clone, Debug)]
pub struct OutboundRequest {
    pub method: &'static str,
    pub url: String,
    pub bearer: Option<RedactedToken>,
    pub json_body: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct RawResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Connect/DNS/TLS/timeout failures.
    #[error("{0}")]
    Connection(String),
    /// The transport refused the request before sending (e.g. cross-origin
    /// redirect, non-HTTPS origin).
    #[error("redirect rejected: {0}")]
    RedirectRejected(String),
}

#[async_trait::async_trait]
pub trait HttpTransport: Send + Sync {
    async fn send(&self, request: OutboundRequest) -> Result<RawResponse, TransportError>;
}

#[async_trait::async_trait]
impl<T: HttpTransport + ?Sized> HttpTransport for Arc<T> {
    async fn send(&self, request: OutboundRequest) -> Result<RawResponse, TransportError> {
        (**self).send(request).await
    }
}

/// Wire operations on the pinned `v0.1.138` contract. All methods are
/// strict: envelope violations are `AgentFieldError::RemoteProtocol`.
#[async_trait::async_trait]
pub trait AgentFieldClient: Send + Sync {
    async fn discovery(&self) -> Result<DiscoveryEnvelope, AgentFieldError>;
    async fn start_async(
        &self,
        execute_target: &str,
        input: &Value,
    ) -> Result<AsyncStartEnvelope, AgentFieldError>;
    async fn status(&self, execution_id: &str) -> Result<StatusEnvelope, AgentFieldError>;
    /// Returns `Ok(None)` when the remote answered 409 `invalid_state`
    /// (already terminal); `Ok(Some(..))` on a confirmed cancellation.
    async fn cancel(
        &self,
        execution_id: &str,
        reason: &str,
    ) -> Result<Option<CancelSuccessEnvelope>, AgentFieldError>;
}

/// HTTP adapter over a configured origin. `T` is the transport so tests can
/// substitute a fake without sockets.
pub struct HttpAgentFieldClient<T: HttpTransport> {
    origin: ControlPlaneOrigin,
    token: Option<RedactedToken>,
    transport: T,
}

impl<T: HttpTransport> HttpAgentFieldClient<T> {
    pub fn new(origin: ControlPlaneOrigin, token: Option<RedactedToken>, transport: T) -> Self {
        Self {
            origin,
            token,
            transport,
        }
    }

    fn url(&self, path: &str) -> String {
        let base = self.origin.base.as_str().trim_end_matches('/');
        format!("{base}{path}")
    }

    async fn send_json(
        &self,
        method: &'static str,
        path: &str,
        body: Option<Value>,
        expected_status: u16,
    ) -> Result<Value, AgentFieldError> {
        let response = self
            .transport
            .send(OutboundRequest {
                method,
                url: self.url(path),
                bearer: self.token.clone(),
                json_body: body,
            })
            .await
            .map_err(|error| match error {
                TransportError::Connection(message) => AgentFieldError::Unavailable(message),
                TransportError::RedirectRejected(message) => {
                    AgentFieldError::Unavailable(format!("redirect rejected: {message}"))
                }
            })?;
        let value = self.check_transport_contract(&response)?;
        if response.status != expected_status {
            return Err(AgentFieldError::RemoteProtocol(format!(
                "pinned contract expects HTTP {expected_status}, got {}",
                response.status
            )));
        }
        Ok(value)
    }

    /// Shared transport-layer contract: credential mapping, JSON
    /// content-type, body cap, and JSON body parse. The pinned per-operation
    /// success status is enforced by the callers.
    fn check_transport_contract(&self, response: &RawResponse) -> Result<Value, AgentFieldError> {
        if response.status == 401 || response.status == 403 {
            return Err(AgentFieldError::Unauthorized);
        }
        if response.status == 429 || response.status >= 500 {
            return Err(AgentFieldError::Unavailable(format!(
                "control plane answered {}",
                response.status
            )));
        }
        let content_type = response.content_type.clone().unwrap_or_default();
        if !content_type.starts_with("application/json") {
            return Err(AgentFieldError::RemoteProtocol(format!(
                "expected application/json response, got `{}`",
                content_type
            )));
        }
        if response.body.len() > MAX_RESPONSE_BODY_BYTES {
            return Err(AgentFieldError::BodyTooLarge(MAX_RESPONSE_BODY_BYTES));
        }
        serde_json::from_slice::<Value>(&response.body)
            .map_err(|error| AgentFieldError::RemoteProtocol(format!("invalid JSON body: {error}")))
    }
}

#[async_trait::async_trait]
impl<T: HttpTransport + 'static> AgentFieldClient for HttpAgentFieldClient<T> {
    async fn discovery(&self) -> Result<DiscoveryEnvelope, AgentFieldError> {
        let envelope = self
            .send_json("GET", "/api/v1/discovery/capabilities", None, 200)
            .await?;
        let decoded =
            DiscoveryEnvelope::decode(&envelope).map_err(AgentFieldError::RemoteProtocol)?;
        // Pinned-version compatibility: any other remote version fails closed.
        for agent in &decoded.capabilities {
            if agent.version != PINNED_AGENTFIELD_VERSION {
                return Err(AgentFieldError::RemoteProtocol(format!(
                    "remote agent `{}` reports version `{}`, pinned contract is {PINNED_AGENTFIELD_VERSION}",
                    agent.agent_id, agent.version
                )));
            }
        }
        Ok(decoded)
    }

    async fn start_async(
        &self,
        execute_target: &str,
        input: &Value,
    ) -> Result<AsyncStartEnvelope, AgentFieldError> {
        // Full atom validation before the URL exists: `%`, extra dots,
        // non-ASCII, and inconsistent separators are rejected with zero
        // transport requests (spec §6.2, AC-13).
        crate::agentfield::config::validate_execute_target(execute_target)
            .map_err(AgentFieldError::RemoteProtocol)?;
        let response = self
            .send_json(
                "POST",
                &format!("/api/v1/execute/async/{execute_target}"),
                Some(input.clone()),
                // Pinned contract: async enqueue answers 202, never 200.
                202,
            )
            .await?;
        if response.get("error").is_some() && response.get("execution_id").is_none() {
            // Remote policy rejection carries a structured error envelope.
            return Err(AgentFieldError::RemoteDenied);
        }
        let decoded =
            AsyncStartEnvelope::decode(&response).map_err(AgentFieldError::RemoteProtocol)?;
        if decoded.target != execute_target {
            return Err(AgentFieldError::RemoteProtocol(format!(
                "async envelope target `{}` does not match the called target `{execute_target}`",
                decoded.target
            )));
        }
        if decoded.status != crate::agentfield::types::RemoteExecutionStatus::Queued {
            return Err(AgentFieldError::RemoteProtocol(format!(
                "async start status must be `queued`, got `{}`",
                decoded.status.as_str()
            )));
        }
        Ok(decoded)
    }

    async fn status(&self, execution_id: &str) -> Result<StatusEnvelope, AgentFieldError> {
        let envelope = self
            .send_json(
                "GET",
                &format!("/api/v1/executions/{execution_id}"),
                None,
                200,
            )
            .await?;
        let decoded = StatusEnvelope::decode(&envelope).map_err(AgentFieldError::RemoteProtocol)?;
        // The status envelope must echo the requested execution ID; a
        // foreign ID is a protocol violation, never silently accepted.
        if decoded.execution_id != execution_id {
            return Err(AgentFieldError::RemoteProtocol(format!(
                "status envelope execution_id `{}` does not match the requested `{execution_id}`",
                decoded.execution_id
            )));
        }
        Ok(decoded)
    }

    async fn cancel(
        &self,
        execution_id: &str,
        reason: &str,
    ) -> Result<Option<CancelSuccessEnvelope>, AgentFieldError> {
        // Cancel shares the exact transport contract of every other
        // operation (401/403, JSON content-type, body cap); only the pinned
        // success/conflict status pair differs.
        let response = self
            .transport
            .send(OutboundRequest {
                method: "POST",
                url: self.url(&format!("/api/v1/executions/{execution_id}/cancel")),
                bearer: self.token.clone(),
                json_body: Some(serde_json::json!({ "reason": reason })),
            })
            .await
            .map_err(|error| match error {
                TransportError::Connection(message) => AgentFieldError::Unavailable(message),
                TransportError::RedirectRejected(message) => {
                    AgentFieldError::Unavailable(format!("redirect rejected: {message}"))
                }
            })?;
        let parsed = self.check_transport_contract(&response)?;
        match response.status {
            200 => {
                let decoded = CancelSuccessEnvelope::decode(&parsed)
                    .map_err(AgentFieldError::RemoteProtocol)?;
                if decoded.execution_id != execution_id {
                    return Err(AgentFieldError::RemoteProtocol(format!(
                        "cancel envelope execution_id `{}` does not match `{execution_id}`",
                        decoded.execution_id
                    )));
                }
                // A confirmed cancellation must answer `cancelled`; a running
                // or any other status is a protocol violation, not a success.
                if decoded.status != crate::agentfield::types::RemoteExecutionStatus::Cancelled {
                    return Err(AgentFieldError::RemoteProtocol(format!(
                        "cancel success status must be `cancelled`, got `{}`",
                        decoded.status.as_str()
                    )));
                }
                Ok(Some(decoded))
            }
            409 => {
                let decoded = CancelConflictEnvelope::decode(&parsed)
                    .map_err(AgentFieldError::RemoteProtocol)?;
                if decoded.error != "invalid_state" {
                    return Err(AgentFieldError::RemoteProtocol(format!(
                        "unexpected cancel conflict error `{}`",
                        decoded.error
                    )));
                }
                Ok(None)
            }
            404 => Err(AgentFieldError::RemoteProtocol(
                "execution not found on cancel".to_string(),
            )),
            other => Err(AgentFieldError::Unavailable(format!(
                "cancel answered {other}"
            ))),
        }
    }
}

/// Production transport over `reqwest` with the frozen network contract:
/// DNS resolved once and pinned for every connection (no rebinding window),
/// the existing `lato-mcp` SSRF/private-network policy applied to exactly
/// those resolved addresses, same-origin redirects only, bounded timeout,
/// bounded body.
#[derive(Debug)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Build the production transport for a validated origin: resolve DNS
    /// once, re-check the resolved addresses through the existing
    /// `lato-mcp` network policy, and pin them onto the client so connection
    /// reuse cannot bypass the re-check (spec §6.1/§8.3).
    pub async fn connect(origin: &ControlPlaneOrigin) -> Result<Self, String> {
        Self::connect_with_resolver(origin, &lato_mcp::SystemMcpDnsResolver).await
    }

    /// [`connect`] with an injectable resolver (test seam).
    pub async fn connect_with_resolver(
        origin: &ControlPlaneOrigin,
        resolver: &dyn lato_mcp::McpDnsResolver,
    ) -> Result<Self, String> {
        use lato_mcp::validate_mcp_url;

        /// Feeds an already-resolved address set to the policy check so the
        /// validated addresses are exactly the pinned ones.
        struct ResolvedAddrsResolver(Vec<std::net::SocketAddr>);

        #[async_trait::async_trait]
        impl lato_mcp::McpDnsResolver for ResolvedAddrsResolver {
            async fn resolve(
                &self,
                _host: &str,
                _port: u16,
            ) -> std::io::Result<Vec<std::net::SocketAddr>> {
                Ok(self.0.clone())
            }
        }

        let host = origin
            .base
            .host_str()
            .ok_or_else(|| "origin has no host".to_string())?
            .to_owned();
        let port = origin
            .base
            .port_or_known_default()
            .ok_or_else(|| "origin has no port".to_string())?;
        // AgentField loopback gate (spec §6.1): loopback destinations are
        // only allowed in explicit development mode, regardless of scheme.
        // The URL literal is checked before any DNS work, and the resolved
        // addresses are re-checked afterwards so DNS rebinding to loopback
        // fails closed too. (`validate_mcp_url` alone permits HTTPS to
        // loopback — MCP semantics — which AgentField must not.)
        ensure_loopback_policy(origin, &host)?;
        // One DNS resolution; policy re-check and pinning share it.
        let resolved = resolver
            .resolve(&host, port)
            .await
            .map_err(|error| format!("DNS resolution failed for `{host}`: {error}"))?;
        if resolved.is_empty() {
            return Err(format!("DNS resolution returned no addresses for `{host}`"));
        }
        ensure_resolved_loopback_policy(origin, &resolved)?;
        // Plain HTTP already only reaches here for loopback dev origins
        // (config-level gate); the policy check re-asserts it on the actual
        // addresses, and HTTPS destinations reject private/link-local/CGNAT.
        validate_mcp_url(&origin.base, &ResolvedAddrsResolver(resolved.clone()))
            .await
            .map_err(|error| format!("origin rejected by the network policy: {error}"))?;
        Self::with_pinned_addrs(origin, &resolved).await
    }

    /// Build the transport with an explicit pinned address set (test seam;
    /// production must use [`connect`]).
    pub async fn with_pinned_addrs(
        origin: &ControlPlaneOrigin,
        resolved: &[std::net::SocketAddr],
    ) -> Result<Self, String> {
        let expected_origin = origin_key(&origin.base);
        let allowed_origin = expected_origin.clone();
        let host = origin
            .base
            .host_str()
            .ok_or_else(|| "origin has no host".to_string())?
            .to_owned();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() > 3 {
                    return attempt.error("too many redirects");
                }
                let target = attempt.url().clone();
                if origin_key(&target) == allowed_origin {
                    attempt.follow()
                } else {
                    let message = format!("cross-origin redirect to `{target}`");
                    attempt.error(TransportError::RedirectRejected(message))
                }
            }))
            .resolve_to_addrs(&host, resolved)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| format!("HTTP client build failed: {error}"))?;
        Ok(Self { client })
    }
}

/// AgentField loopback gate on the URL literal (spec §6.1): `localhost`,
/// `127.0.0.0/8`, and `::1` hosts are only allowed when the origin carries
/// the explicit development flag. Checked before any DNS work.
fn ensure_loopback_policy(origin: &ControlPlaneOrigin, host: &str) -> Result<(), String> {
    let loopback_literal = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    if loopback_literal && !origin.loopback_dev_mode {
        return Err(format!(
            "loopback destination `{host}` is only allowed in explicit development mode"
        ));
    }
    Ok(())
}

/// AgentField loopback gate on the resolved addresses (spec §6.1): a
/// production origin whose DNS resolution lands on loopback (including
/// rebinding) fails closed. Dev-mode origins require every address to be
/// loopback, which `validate_mcp_url` enforces for plain HTTP.
fn ensure_resolved_loopback_policy(
    origin: &ControlPlaneOrigin,
    resolved: &[SocketAddr],
) -> Result<(), String> {
    if !origin.loopback_dev_mode && resolved.iter().any(|addr| addr.ip().is_loopback()) {
        return Err(
            "resolved addresses include loopback; loopback destinations are only allowed in \
             explicit development mode"
                .to_string(),
        );
    }
    Ok(())
}

fn origin_key(url: &reqwest::Url) -> String {
    format!(
        "{}://{}:{}",
        url.scheme(),
        url.host_str().unwrap_or_default().to_lowercase(),
        url.port_or_known_default().unwrap_or(0)
    )
}

#[async_trait::async_trait]
impl HttpTransport for ReqwestTransport {
    async fn send(&self, request: OutboundRequest) -> Result<RawResponse, TransportError> {
        let url: reqwest::Url = request
            .url
            .parse()
            .map_err(|error| TransportError::Connection(format!("invalid url: {error}")))?;
        let mut builder = self.client.request(
            reqwest::Method::from_bytes(request.method.as_bytes()).unwrap(),
            url,
        );
        if let Some(bearer) = &request.bearer {
            builder = builder.bearer_auth(bearer.expose());
        }
        if let Some(body) = &request.json_body {
            builder = builder.json(body);
        }
        let mut response = builder
            .send()
            .await
            .map_err(|error| TransportError::Connection(error.to_string()))?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_string());
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| TransportError::Connection(error.to_string()))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE_BODY_BYTES {
                return Err(TransportError::Connection(format!(
                    "response body exceeds {} bytes",
                    MAX_RESPONSE_BODY_BYTES
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(RawResponse {
            status: response.status().as_u16(),
            content_type,
            body,
        })
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use std::net::{IpAddr, SocketAddr};

    struct FakeResolver(Vec<IpAddr>);

    #[async_trait::async_trait]
    impl lato_mcp::McpDnsResolver for FakeResolver {
        async fn resolve(&self, _host: &str, _port: u16) -> std::io::Result<Vec<SocketAddr>> {
            Ok(self.0.iter().map(|ip| SocketAddr::new(*ip, 443)).collect())
        }
    }

    fn origin(url: &str, dev_mode: bool) -> ControlPlaneOrigin {
        ControlPlaneOrigin {
            base: url.parse().unwrap(),
            loopback_dev_mode: dev_mode,
        }
    }

    fn public() -> IpAddr {
        "93.184.216.34".parse().unwrap()
    }

    fn private() -> IpAddr {
        "10.20.30.40".parse().unwrap()
    }

    fn loopback() -> IpAddr {
        "127.0.0.1".parse().unwrap()
    }

    #[tokio::test]
    async fn https_private_addresses_are_rejected_before_client_build() {
        let error = ReqwestTransport::connect_with_resolver(
            &origin("https://agents.example.internal", false),
            &FakeResolver(vec![private()]),
        )
        .await
        .unwrap_err();
        assert!(error.contains("network policy"), "{error}");
    }

    #[tokio::test]
    async fn plain_http_without_loopback_is_rejected_at_transport_layer() {
        // Even if a misconfigured origin slipped past config, the transport
        // refuses plain HTTP to non-loopback addresses.
        let error = ReqwestTransport::connect_with_resolver(
            &origin("http://agents.example.internal", true),
            &FakeResolver(vec![public()]),
        )
        .await
        .unwrap_err();
        assert!(error.contains("network policy"), "{error}");
    }

    #[tokio::test]
    async fn loopback_http_connects_and_pins_the_resolved_addresses() {
        let transport = ReqwestTransport::connect_with_resolver(
            &origin("http://127.0.0.1:8080", true),
            &FakeResolver(vec![loopback()]),
        )
        .await
        .unwrap();
        // The transport exists and is usable; the pinned client was built.
        let _ = transport.client;
    }

    #[tokio::test]
    async fn dns_failure_and_empty_resolution_fail_closed() {
        struct FailingResolver;
        #[async_trait::async_trait]
        impl lato_mcp::McpDnsResolver for FailingResolver {
            async fn resolve(&self, _host: &str, _port: u16) -> std::io::Result<Vec<SocketAddr>> {
                Err(std::io::Error::other("dns down"))
            }
        }
        let error = ReqwestTransport::connect_with_resolver(
            &origin("https://agents.example.internal", false),
            &FailingResolver,
        )
        .await
        .unwrap_err();
        assert!(error.contains("DNS resolution failed"), "{error}");

        let error = ReqwestTransport::connect_with_resolver(
            &origin("https://agents.example.internal", false),
            &FakeResolver(vec![]),
        )
        .await
        .unwrap_err();
        assert!(error.contains("no addresses"), "{error}");
    }

    #[tokio::test]
    async fn policy_review_covers_exactly_the_pinned_address_set() {
        // A mixed public+private resolution must be rejected: the policy sees
        // the same set the connection would use, so reuse cannot bypass it.
        let error = ReqwestTransport::connect_with_resolver(
            &origin("https://agents.example.internal", false),
            &FakeResolver(vec![public(), private()]),
        )
        .await
        .unwrap_err();
        assert!(error.contains("network policy"), "{error}");
    }
}

#[cfg(test)]
mod round2_loopback_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    struct FixedResolver(Vec<IpAddr>);

    #[async_trait::async_trait]
    impl lato_mcp::McpDnsResolver for FixedResolver {
        async fn resolve(&self, _host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
            Ok(self.0.iter().map(|ip| SocketAddr::new(*ip, port)).collect())
        }
    }

    fn origin(url: &str) -> ControlPlaneOrigin {
        ControlPlaneOrigin {
            base: url.parse().unwrap(),
            loopback_dev_mode: false,
        }
    }

    #[tokio::test]
    async fn production_https_localhost_literal_is_rejected() {
        let error = ReqwestTransport::connect_with_resolver(
            &origin("https://localhost"),
            &FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        )
        .await
        .unwrap_err();
        assert!(error.contains("loopback"), "{error}");
    }

    #[tokio::test]
    async fn production_https_ipv6_loopback_literal_and_resolution_are_rejected() {
        let error = ReqwestTransport::connect_with_resolver(
            &origin("https://[::1]"),
            &FixedResolver(vec![IpAddr::V6(Ipv6Addr::LOCALHOST)]),
        )
        .await
        .unwrap_err();
        assert!(error.contains("loopback"), "{error}");
        let error = ReqwestTransport::connect_with_resolver(
            &origin("https://agents.example.internal"),
            &FixedResolver(vec![IpAddr::V6(Ipv6Addr::LOCALHOST)]),
        )
        .await
        .unwrap_err();
        assert!(error.contains("loopback"), "{error}");
    }

    #[tokio::test]
    async fn production_https_resolving_to_mixed_loopback_is_rejected() {
        let error = ReqwestTransport::connect_with_resolver(
            &origin("https://agents.example.internal"),
            &FixedResolver(vec![
                "93.184.216.34".parse().unwrap(),
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            ]),
        )
        .await
        .unwrap_err();
        assert!(error.contains("loopback"), "{error}");
    }
}
