// Phase 7C1.1: AgentField remote-execution adapter — configuration, strict
// wire contract for the pinned `v0.1.138`, injectable transport client, the
// production HTTPS transport with its frozen network policy (7C1.1:
// `transport`, unique policy-enforcing factory, DNS classification and
// address pinning), snapshot state machine, and doctor diagnostics.
//
// Phase 7C2 adds: the session-frozen catalog revision (`catalog`), the
// session-scoped run manager (`manager`), and the main-session model tool
// (`tool`). Still absent (7C3): journal events and cross-process
// resume/reconcile.

pub mod catalog;
pub mod client;
pub mod config;
pub mod manager;
pub mod probe;
#[cfg(test)]
mod probes_7c1_1;
pub mod tool;
pub mod transport;
pub mod types;

pub use catalog::AgentFieldCatalog;
pub use client::{
    AgentFieldClient, AgentFieldError, HttpAgentFieldClient, HttpTransport,
    MAX_RESPONSE_BODY_BYTES, OutboundRequest, RawResponse, RedactedToken, TransportError,
};
pub use config::{
    CapabilityConfig, ConfigError, ControlPlaneOrigin, MAX_CAPABILITIES, MAX_INPUT_BYTES,
    MAX_OUTPUT_BYTES, PINNED_AGENTFIELD_VERSION,
};
pub use manager::{AgentFieldManager, CancelOutcome, ManagerError, RunState, RunStatus};
pub use probe::{
    AgentFieldHealthSnapshot, AgentFieldProbe, AgentFieldProbeCache, HEALTH_SNAPSHOT_TTL,
};
pub use tool::{AgentFieldTool, SessionAgentFieldHandle};
pub use transport::ReqwestTransport;
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

/// Load and validate the `agentfield` stanza from `$LATO_HOME/config.json`
/// (7C2 registration gate). Returns `None` — i.e. zero tool registration
/// and zero network — when the file is missing, the stanza is absent,
/// `enabled` is false, or validation fails (doctor reports the details).
pub fn load_agentfield_config(home: &std::path::Path) -> Option<config::AgentFieldConfig> {
    let bytes = std::fs::read(home.join("config.json")).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let raw = value.get("agentfield")?;
    config::AgentFieldConfig::parse(raw).ok().flatten()
}

/// The unique policy-enforcing production factory (7C1.1): every product
/// path obtains its AgentField client here and nowhere else. The credential
/// is resolved BEFORE any transport exists — an unresolvable reference
/// yields `agentfield.unconfigured` with zero network requests — and the
/// transport is built through `ReqwestTransport::connect`, the only
/// constructor that enforces the frozen network policy.
pub async fn production_agentfield_client(
    store: Option<&lato_ai::CredentialStore>,
    config: &config::AgentFieldConfig,
) -> Result<client::HttpAgentFieldClient<transport::ReqwestTransport>, client::AgentFieldError> {
    let credential = resolve_agentfield_credential(store, &config.credential_reference)
        .ok_or_else(|| {
            client::AgentFieldError::CredentialMissing(config.credential_reference.clone())
        })?;
    let transport = transport::ReqwestTransport::connect(&config.origin)
        .await
        .map_err(client::map_transport_error)?;
    Ok(client::HttpAgentFieldClient::new(
        config.origin.clone(),
        Some(credential),
        transport,
    ))
}

/// 7C2 variant of the policy factory for already-resolved credentials: the
/// registration gate resolves the credential BEFORE the transport exists
/// (unresolvable ⇒ zero registration, zero network), so this path receives
/// the redacted token and enforces the same frozen transport policy through
/// `ReqwestTransport::connect`.
pub async fn production_agentfield_client_from_token(
    credential: client::RedactedToken,
    config: &config::AgentFieldConfig,
) -> Result<client::HttpAgentFieldClient<transport::ReqwestTransport>, client::AgentFieldError> {
    let transport = transport::ReqwestTransport::connect(&config.origin)
        .await
        .map_err(client::map_transport_error)?;
    Ok(client::HttpAgentFieldClient::new(
        config.origin.clone(),
        Some(credential),
        transport,
    ))
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

    /// 7C1.1 AC-07: the token AND the full `Authorization` header value must
    /// be absent from every printable surface (Debug/Display of the token,
    /// request, transport and client errors, and health snapshots). Journal
    /// and doctor surfaces are covered by their own integration tests.
    #[test]
    fn credential_and_authorization_value_hit_zero_surfaces() {
        const SECRET: &str = "sk-live-secret-7c11-redaction-probe";
        let auth_value = format!("Bearer {SECRET}");
        let token = client::RedactedToken::new(SECRET);
        let request = client::OutboundRequest {
            method: "GET",
            url: "https://agents.example.com/api/v1/discovery/capabilities".into(),
            bearer: Some(token.clone()),
            json_body: None,
        };
        let errors = [
            format!(
                "{:?}",
                client::TransportError::Connection("connect failed".into())
            ),
            format!(
                "{}",
                client::TransportError::Connection("connect failed".into())
            ),
            format!("{:?}", client::AgentFieldError::Unauthorized),
            format!("{}", client::AgentFieldError::Unauthorized),
            format!(
                "{:?}",
                client::AgentFieldError::Unavailable("control plane down".into())
            ),
            format!(
                "{}",
                client::AgentFieldError::Unavailable("control plane down".into())
            ),
            format!(
                "{:?}",
                client::AgentFieldError::RemoteProtocol("bad envelope".into())
            ),
            format!(
                "{}",
                client::AgentFieldError::RemoteProtocol("bad envelope".into())
            ),
            format!(
                "{:?}",
                client::AgentFieldError::CredentialMissing("agentfield:primary".into())
            ),
            format!(
                "{}",
                client::AgentFieldError::CredentialMissing("agentfield:primary".into())
            ),
        ];
        let surfaces = [
            format!("{token:?}"),
            format!("{token}"),
            format!("{request:?}"),
            errors.join("\n"),
        ];
        for surface in &surfaces {
            assert!(!surface.contains(SECRET), "token leaked: {surface}");
            assert!(
                !surface.contains(&auth_value),
                "full Authorization value leaked: {surface}"
            );
            assert!(
                !surface.to_lowercase().contains("authorization: bearer"),
                "{surface}"
            );
        }
    }

    #[tokio::test]
    async fn unresolvable_credential_means_zero_network_and_a_reference_only_error() {
        let config = config::AgentFieldConfig::parse(&serde_json::json!({
            "enabled": true,
            "baseUrl": "https://agents.example.internal",
            "credential": "agentfield:primary",
        }))
        .unwrap()
        .unwrap();
        // No store, no env: the factory must fail before any DNS/connect.
        let error = match production_agentfield_client(None, &config).await {
            Err(error) => error,
            Ok(_) => panic!("unresolvable credential must not produce a client"),
        };
        assert!(matches!(
            error,
            client::AgentFieldError::CredentialMissing(_)
        ));
        let message = error.to_string();
        assert!(message.contains("agentfield:primary"));
        assert!(!message.contains("https://agents.example.internal"));
    }
}
