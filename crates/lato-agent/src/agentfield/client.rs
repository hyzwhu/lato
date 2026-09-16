// Phase 7C1 (v1.2.1): `AgentFieldClient` trait and the offline strict HTTP
// adapter seam. There is no production transport in this slice: unit tests
// drive a fake transport, and the real-network production implementation is
// deferred to Phase 7C1.1 (spec §0.3). The public API exposes no way to
// reach a real network from this module.

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
