// Phase 7C1 (v1.2.1): AgentField OFFLINE-ONLY remote-execution adapter
// foundation — configuration, strict wire contract for the pinned `v0.1.138`,
// injectable fake-transport client trait, snapshot state machine, and static
// doctor diagnostics. Per spec §0.3 there is deliberately NO production
// transport, no DNS/address policy, and no public network seam in this
// slice; those arrive in Phase 7C1.1.
//
// Also absent (7C2/7C3): model tool registration, `AgentFieldManager`,
// policy/approval integration, journal events, and cross-process
// resume/reconcile.

pub mod client;
pub mod config;
pub mod probe;
pub(crate) mod transport;
pub mod types;

pub use client::{
    AgentFieldClient, AgentFieldError, HttpAgentFieldClient, HttpTransport,
    MAX_RESPONSE_BODY_BYTES, OutboundRequest, RawResponse, RedactedToken, TransportError,
};
pub use config::{
    CapabilityConfig, ConfigError, ControlPlaneOrigin, MAX_CAPABILITIES, MAX_INPUT_BYTES,
    MAX_OUTPUT_BYTES, PINNED_AGENTFIELD_VERSION,
};
pub use probe::{
    AgentFieldHealthSnapshot, AgentFieldProbe, AgentFieldProbeCache, HEALTH_SNAPSHOT_TTL,
};
pub use types::{
    AsyncStartEnvelope, CancelConflictEnvelope, CancelSuccessEnvelope, DiscoveryAgent,
    DiscoveryEnvelope, DiscoveryReasoner, RemoteExecutionStatus, StatusEnvelope,
    StatusOptionalFields,
};

/// Resolve the credential reference `agentfield:<key>` to a Bearer token:
///
/// 1. the Lato credential store entry `agentfield` (`Credential::ApiKey`);
/// 2. the environment variable `LATO_AGENTFIELD_CREDENTIAL`.
///
/// The value is wrapped in a redacted token and never logged or journaled.
pub fn resolve_agentfield_credential(
    store: Option<&lato_ai::CredentialStore>,
    reference: &str,
) -> Option<client::RedactedToken> {
    let key = reference.strip_prefix("agentfield:")?;
    if key.is_empty() {
        return None;
    }
    if let Some(store) = store
        && let Some(credential) = store.get("agentfield")
        && let lato_ai::Credential::ApiKey { key: token } = credential
    {
        return Some(client::RedactedToken::new(token));
    }
    std::env::var("LATO_AGENTFIELD_CREDENTIAL")
        .ok()
        .filter(|value| !value.is_empty())
        .map(client::RedactedToken::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_token_never_prints_its_secret() {
        let token = client::RedactedToken::new("sk-live-123");
        assert!(!format!("{token:?}").contains("sk-live-123"));
        assert!(!format!("{token}").contains("sk-live-123"));
    }
}
