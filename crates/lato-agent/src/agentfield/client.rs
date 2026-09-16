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
use std::sync::Arc;

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
            .send_json("GET", "/api/v1/discovery/capabilities", None)
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
            .send_json("GET", &format!("/api/v1/executions/{execution_id}"), None)
            .await?;
        StatusEnvelope::decode(&envelope).map_err(AgentFieldError::RemoteProtocol)
    }

    async fn cancel(
        &self,
        execution_id: &str,
        reason: &str,
    ) -> Result<Option<CancelSuccessEnvelope>, AgentFieldError> {
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
        if response.status == 401 || response.status == 403 {
            return Err(AgentFieldError::Unauthorized);
        }
        if response.status >= 500 || response.status == 429 {
            return Err(AgentFieldError::Unavailable(format!(
                "control plane answered {}",
                response.status
            )));
        }
        let parsed: Value = serde_json::from_slice(&response.body).map_err(|error| {
            AgentFieldError::RemoteProtocol(format!("invalid JSON body: {error}"))
        })?;
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
/// same-origin redirects only, bounded timeout, bounded body.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new(origin: &ControlPlaneOrigin) -> Result<Self, String> {
        let expected_origin = origin_key(&origin.base);
        let allowed_origin = expected_origin.clone();
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
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| format!("HTTP client build failed: {error}"))?;
        Ok(Self { client })
    }
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
