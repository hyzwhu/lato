// Phase 7C1: AgentField remote-execution adapter foundation — configuration,
// strict wire contract for the pinned `v0.1.138`, client trait + HTTP
// adapter, and doctor diagnostics.
//
// Deliberately absent in this slice (7C2/7C3): model tool registration,
// `AgentFieldManager`, policy/approval integration, journal events, and
// cross-process resume/reconcile.

pub mod client;
pub mod config;
pub mod probe;
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

use config::AgentFieldConfig;

/// Doctor diagnostic state for the optional adapter (spec §10.2/§12.3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentFieldDoctorState {
    /// No `agentfield` stanza in settings: feature absent, tool unregistered.
    Disabled,
    /// Parsed and enabled but the credential reference is not resolvable.
    Unconfigured { detail: String },
    /// Enabled + configured, but the pinned control plane could not be
    /// reached (offline doctor or genuine outage).
    Unavailable { detail: String },
    /// Reached, but version/envelope/target contract violated.
    ContractMismatch { detail: String },
    /// Enabled, configured, probed, pinned contract verified.
    Enabled { detail: String },
}

impl AgentFieldDoctorState {
    pub fn summary(&self) -> (&'static str, String) {
        match self {
            Self::Disabled => ("disabled", "agentfield not configured".to_string()),
            Self::Unconfigured { detail } => ("unconfigured", detail.clone()),
            Self::Unavailable { detail } => ("unavailable", detail.clone()),
            Self::ContractMismatch { detail } => ("contract-mismatch", detail.clone()),
            Self::Enabled { detail } => ("enabled", detail.clone()),
        }
    }
}

/// Classify a live discovery probe against the pinned contract.
pub fn classify_probe(
    config: Option<&AgentFieldConfig>,
    probe: Result<&DiscoveryEnvelope, &AgentFieldError>,
) -> AgentFieldDoctorState {
    let Some(config) = config else {
        return AgentFieldDoctorState::Disabled;
    };
    if !config.enabled {
        return AgentFieldDoctorState::Disabled;
    }
    match probe {
        Err(AgentFieldError::Unauthorized) => AgentFieldDoctorState::Unconfigured {
            detail: "control plane rejected the credential; refresh the referenced credential"
                .to_string(),
        },
        Err(AgentFieldError::RemoteProtocol(detail)) => AgentFieldDoctorState::ContractMismatch {
            detail: detail.clone(),
        },
        Err(other) => AgentFieldDoctorState::Unavailable {
            detail: other.to_string(),
        },
        Ok(envelope) => {
            // Every configured allowlist target must still exist remotely; the
            // probe may not expand the allowlist, only confirm it.
            for (alias, capability) in &config.capabilities {
                let found = envelope.capabilities.iter().any(|agent| {
                    agent.health_status == "healthy"
                        && agent.reasoners.iter().any(|reasoner| {
                            crate::agentfield::config::derive_execute_target(
                                &agent.agent_id,
                                &reasoner.id,
                                &reasoner.invocation_target,
                            )
                            .as_deref()
                                == Ok(capability.target.as_str())
                        })
                });
                if !found {
                    return AgentFieldDoctorState::ContractMismatch {
                        detail: format!(
                            "capability `{alias}` target `{}` is missing or unhealthy on the pinned control plane",
                            capability.target
                        ),
                    };
                }
            }
            AgentFieldDoctorState::Enabled {
                detail: format!(
                    "pinned {PINNED_AGENTFIELD_VERSION} contract verified; {} capability(ies)",
                    config.capabilities.len()
                ),
            }
        }
    }
}

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
    use crate::agentfield::config::CapabilityConfig;

    fn config_with(target: &str) -> AgentFieldConfig {
        AgentFieldConfig {
            enabled: true,
            origin: ControlPlaneOrigin {
                base: "https://agents.example.internal/".parse().unwrap(),
                loopback_dev_mode: false,
            },
            credential_reference: "agentfield:primary".into(),
            capabilities: vec![(
                "contract-review".into(),
                CapabilityConfig {
                    target: target.into(),
                    description: "d".into(),
                    input_schema: serde_json::json!({}),
                    risk: "remote_read".into(),
                    timeout_seconds: 900,
                    max_output_bytes: 65536,
                    input_schema_camel: None,
                },
            )],
        }
    }

    fn discovery_with(target: &str, version: &str, health: &str) -> DiscoveryEnvelope {
        let (agent_id, reasoner_id) = target.split_once('.').expect("dot target");
        DiscoveryEnvelope {
            discovered_at: "2026-09-16T00:00:00Z".into(),
            total_agents: 1,
            total_reasoners: 1,
            total_skills: 0,
            capabilities: vec![DiscoveryAgent {
                agent_id: agent_id.into(),
                group_id: String::new(),
                base_url: "https://agentfield.invalid".into(),
                version: version.into(),
                health_status: health.into(),
                deployment_type: "service".into(),
                last_heartbeat: "2026-09-16T00:00:00Z".into(),
                reasoners: vec![DiscoveryReasoner {
                    id: reasoner_id.into(),
                    invocation_target: format!("{agent_id}:{reasoner_id}"),
                    description: None,
                    tags: None,
                    input_schema: None,
                    output_schema: None,
                    examples: None,
                }],
                skills: serde_json::json!([]),
            }],
        }
    }

    #[test]
    fn classify_reports_disabled_without_config() {
        assert!(matches!(
            classify_probe(
                None,
                Ok(&discovery_with(
                    "legal-agent.review_contract",
                    "v0.1.138",
                    "healthy"
                ))
            ),
            AgentFieldDoctorState::Disabled
        ));
    }

    #[test]
    fn classify_reports_enabled_on_matching_pinned_contract() {
        let config = config_with("legal-agent.review_contract");
        assert!(matches!(
            classify_probe(
                Some(&config),
                Ok(&discovery_with(
                    "legal-agent.review_contract",
                    "v0.1.138",
                    "healthy"
                ))
            ),
            AgentFieldDoctorState::Enabled { .. }
        ));
    }

    #[test]
    fn classify_reports_contract_mismatch_on_missing_unhealthy_or_version_drift() {
        let config = config_with("legal-agent.review_contract");
        // Target missing remotely.
        assert!(matches!(
            classify_probe(
                Some(&config),
                Ok(&discovery_with("other.target", "v0.1.138", "healthy"))
            ),
            AgentFieldDoctorState::ContractMismatch { .. }
        ));
        // Unhealthy agent.
        assert!(matches!(
            classify_probe(
                Some(&config),
                Ok(&discovery_with(
                    "legal-agent.review_contract",
                    "v0.1.138",
                    "unhealthy"
                ))
            ),
            AgentFieldDoctorState::ContractMismatch { .. }
        ));
        // (Version drift is rejected one layer earlier, by the client's
        // pinned-version check — covered by the contract tests.)
        // Probe violation.
        let error = AgentFieldError::RemoteProtocol("bad envelope".into());
        assert!(matches!(
            classify_probe(Some(&config), Err(&error)),
            AgentFieldDoctorState::ContractMismatch { .. }
        ));
    }

    #[test]
    fn classify_reports_unavailable_and_unauthorized() {
        let config = config_with("legal-agent.review_contract");
        let error = AgentFieldError::Unavailable("timeout".into());
        assert!(matches!(
            classify_probe(Some(&config), Err(&error)),
            AgentFieldDoctorState::Unavailable { .. }
        ));
        let error = AgentFieldError::Unauthorized;
        assert!(matches!(
            classify_probe(Some(&config), Err(&error)),
            AgentFieldDoctorState::Unconfigured { .. }
        ));
    }

    #[test]
    fn redacted_token_never_prints_its_secret() {
        let token = client::RedactedToken::new("sk-live-123");
        assert!(!format!("{token:?}").contains("sk-live-123"));
        assert!(!format!("{token}").contains("sk-live-123"));
    }
}
