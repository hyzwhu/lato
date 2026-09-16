// Phase 7C1: AgentField control-plane configuration contract.
//
// Lives under `$LATO_HOME/config.json` as an optional `agentfield` object.
// Parsing is strict about security-relevant properties (URL shape, alias and
// target atoms, caps) and fails closed with the stable
// `agentfield.invalid_arguments` family before anything touches the network.

use serde::Deserialize;
use serde_json::Value;
use url::Url;

pub const MAX_CAPABILITIES: usize = 64;
pub const MAX_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub const PINNED_AGENTFIELD_VERSION: &str = "v0.1.138";

/// Config-level parse/validation failure. The `code` is one of the stable
/// `agentfield.*` codes; message is safe to display (no secrets).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigError {
    pub code: &'static str,
    pub message: String,
}

impl ConfigError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "agentfield.invalid_arguments",
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// A validated control-plane origin: absolute HTTPS (or loopback HTTP in
/// explicit development mode), no userinfo/query/fragment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPlaneOrigin {
    /// `https://host` or `https://host:port` — always ends without a path
    /// beyond the root; API paths are appended by the client.
    pub base: Url,
    /// True when the origin is an explicitly allowed loopback HTTP origin.
    pub loopback_dev_mode: bool,
}

/// One allowlisted capability after validation.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
pub struct CapabilityConfig {
    pub target: String,
    pub description: String,
    pub input_schema: Value,
    pub risk: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_output")]
    pub max_output_bytes: usize,
    #[serde(rename = "inputSchema")]
    pub input_schema_camel: Option<Value>,
}

fn default_timeout() -> u64 {
    900
}

fn default_output() -> usize {
    MAX_OUTPUT_BYTES
}

/// Validated `agentfield` settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentFieldConfig {
    pub enabled: bool,
    pub origin: ControlPlaneOrigin,
    /// Credential reference in the frozen `agentfield:<key>` shape. The key
    /// resolves through the Lato credential store or environment; the secret
    /// value never lives in configuration, journal, logs, or tool output.
    pub credential_reference: String,
    /// Alias → capability, sorted by alias for canonical serialization.
    pub capabilities: Vec<(String, CapabilityConfig)>,
}

/// Raw settings shape (camelCase keys as frozen in the spec example).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgentFieldConfig {
    enabled: bool,
    #[serde(rename = "baseUrl")]
    base_url: String,
    credential: String,
    /// Explicit development escape hatch: allow plain HTTP to a loopback
    /// address. Never honored for non-loopback hosts.
    #[serde(rename = "allowLoopbackHttp", default)]
    allow_loopback_http: bool,
    #[serde(default)]
    capabilities: std::collections::BTreeMap<String, RawCapability>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCapability {
    target: String,
    description: String,
    #[serde(rename = "inputSchema", alias = "input_schema")]
    input_schema: Value,
    risk: String,
    #[serde(rename = "timeoutSeconds", default = "default_timeout")]
    timeout_seconds: u64,
    #[serde(rename = "maxOutputBytes", default = "default_output")]
    max_output_bytes: usize,
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// Strict ASCII atom: `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$`. Rejects `.`, `:`,
/// `/`, `%`, non-ASCII and Unicode confusion characters outright; no percent
/// decoding, no normalization.
pub fn validate_atom(value: &str, field: &str) -> Result<(), String> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && value.starts_with(|c: char| c.is_ascii_alphanumeric())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{field} `{value}` is not a valid AgentField atom (ASCII [A-Za-z0-9][A-Za-z0-9_-]{{0,63}}, no `.`, `:`, `/`, `%`, non-ASCII)"
        ))
    }
}

/// Validate a discovery `(agent_id, reasoner)` pair: both atoms strict, the
/// colon `invocation_target` must match the atoms exactly, and the derived
/// dot execute target is returned. The raw colon target never reaches a URL.
pub fn derive_execute_target(
    agent_id: &str,
    reasoner_id: &str,
    invocation_target: &str,
) -> Result<String, String> {
    validate_atom(agent_id, "discovery agent_id")?;
    validate_atom(reasoner_id, "discovery reasoner id")?;
    let expected = format!("{agent_id}:{reasoner_id}");
    if invocation_target != expected {
        return Err(format!(
            "discovery invocation_target `{invocation_target}` does not equal `{expected}`"
        ));
    }
    Ok(format!("{agent_id}.{reasoner_id}"))
}

/// Validate an already-derived dot execute target (`agent_id.reasoner_id`)
/// before it may touch a URL: exactly one separator, both atoms strict.
/// Rejects `%`, extra dots, non-ASCII, and any percent-encoded separator.
pub fn validate_execute_target(target: &str) -> Result<(), String> {
    let Some((agent_id, reasoner_id)) = target.split_once('.') else {
        return Err(format!(
            "execute target `{target}` must be `agent_id.reasoner_id`"
        ));
    };
    validate_atom(agent_id, "execute target agent_id")?;
    validate_atom(reasoner_id, "execute target reasoner_id")?;
    if target.matches('.').count() != 1 {
        return Err(format!(
            "execute target `{target}` must contain exactly one separator"
        ));
    }
    Ok(())
}

impl AgentFieldConfig {
    /// Parse and validate the raw settings value (`agentfield` key).
    pub fn parse(value: &Value) -> Result<Option<Self>, ConfigError> {
        if value.is_null() {
            return Ok(None);
        }
        let raw: RawAgentFieldConfig = serde_json::from_value(value.clone())
            .map_err(|error| ConfigError::invalid(format!("invalid agentfield config: {error}")))?;
        if !raw.enabled {
            // Even a disabled stanza is parsed so malformed config is reported
            // by doctor instead of being silently ignored.
            let origin = validate_base_url(&raw.base_url, raw.allow_loopback_http)
                .map_err(ConfigError::invalid)?;
            validate_credential(&raw.credential).map_err(ConfigError::invalid)?;
            validate_capabilities(&raw.capabilities).map_err(ConfigError::invalid)?;
            return Ok(Some(Self {
                enabled: false,
                origin,
                credential_reference: raw.credential,
                capabilities: Vec::new(),
            }));
        }
        let origin = validate_base_url(&raw.base_url, raw.allow_loopback_http)
            .map_err(ConfigError::invalid)?;
        validate_credential(&raw.credential).map_err(ConfigError::invalid)?;
        let capabilities =
            validate_capabilities(&raw.capabilities).map_err(ConfigError::invalid)?;
        Ok(Some(Self {
            enabled: true,
            origin,
            credential_reference: raw.credential,
            capabilities,
        }))
    }

    pub fn capability(&self, alias: &str) -> Option<&CapabilityConfig> {
        self.capabilities
            .iter()
            .find(|(existing, _)| existing == alias)
            .map(|(_, capability)| capability)
    }
}

fn validate_base_url(raw: &str, allow_loopback_http: bool) -> Result<ControlPlaneOrigin, String> {
    let parsed =
        Url::parse(raw).map_err(|error| format!("baseUrl `{raw}` is not a URL: {error}"))?;
    if !parsed.query().is_none() {
        return Err("baseUrl must not carry a query string".to_string());
    }
    if !parsed.fragment().is_none() {
        return Err("baseUrl must not carry a fragment".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("baseUrl must not carry userinfo".to_string());
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "baseUrl must include a host".to_string())?;
    let loopback = is_loopback_host(host);
    match parsed.scheme() {
        "https" => {}
        "http" if loopback && allow_loopback_http => {}
        "http" => {
            return Err(
                "baseUrl must use HTTPS (plain HTTP is only allowed for loopback in explicit development mode)"
                    .to_string(),
            );
        }
        other => return Err(format!("baseUrl scheme `{other}` is not supported")),
    }
    // Normalize: scheme + lowercase host + optional port, no path.
    let port = parsed.port();
    let mut base = Url::parse(&format!("{}://{}", parsed.scheme(), host.to_lowercase()))
        .map_err(|error| format!("baseUrl normalization failed: {error}"))?;
    if let Some(port) = port {
        base.set_port(Some(port))
            .map_err(|_| "baseUrl port rejected".to_string())?;
    }
    Ok(ControlPlaneOrigin {
        base,
        loopback_dev_mode: loopback && parsed.scheme() == "http",
    })
}

fn validate_credential(reference: &str) -> Result<(), String> {
    let Some(key) = reference.strip_prefix("agentfield:") else {
        return Err(format!(
            "credential `{reference}` must use the `agentfield:<key>` reference shape"
        ));
    };
    if key.is_empty() || key.len() > 128 {
        return Err("credential key must contain 1..=128 characters".to_string());
    }
    Ok(())
}

fn validate_capabilities(
    capabilities: &std::collections::BTreeMap<String, RawCapability>,
) -> Result<Vec<(String, CapabilityConfig)>, String> {
    if capabilities.len() > MAX_CAPABILITIES {
        return Err(format!(
            "at most {MAX_CAPABILITIES} capabilities are allowed (got {})",
            capabilities.len()
        ));
    }
    let mut validated = Vec::new();
    for (alias, raw) in capabilities {
        let alias_valid = !alias.is_empty()
            && alias.len() <= 64
            && alias.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && alias
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if !alias_valid {
            return Err(format!(
                "capability alias `{alias}` must match [a-z0-9][a-z0-9_-]{{0,63}}"
            ));
        }
        if raw.description.trim().is_empty() {
            return Err(format!(
                "capability `{alias}` needs a non-empty description"
            ));
        }
        if !matches!(raw.input_schema, Value::Object(_)) {
            return Err(format!(
                "capability `{alias}` inputSchema must be a JSON object"
            ));
        }
        let risk_ok = !raw.risk.is_empty()
            && raw.risk.len() <= 32
            && raw.risk.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        if !risk_ok {
            return Err(format!(
                "capability `{alias}` risk `{}` must be lowercase [a-z_] (1..=32 chars)",
                raw.risk
            ));
        }
        if raw.timeout_seconds == 0 || raw.timeout_seconds > 3600 {
            return Err(format!(
                "capability `{alias}` timeoutSeconds must be 1..=3600"
            ));
        }
        if raw.max_output_bytes == 0 || raw.max_output_bytes > MAX_OUTPUT_BYTES {
            return Err(format!(
                "capability `{alias}` maxOutputBytes must be 1..={MAX_OUTPUT_BYTES}"
            ));
        }
        // The allowlist target is the locally configured authority: it must
        // already be a derived dot execute target of valid atoms.
        let (agent_id, reasoner_id) = raw.target.rsplit_once('.').ok_or_else(|| {
            format!(
                "capability `{alias}` target `{}` must be `agent.reasoner`",
                raw.target
            )
        })?;
        validate_atom(agent_id, &format!("capability `{alias}` target agent"))?;
        validate_atom(
            reasoner_id,
            &format!("capability `{alias}` target reasoner"),
        )?;
        if raw.target.matches('.').count() != 1 {
            return Err(format!(
                "capability `{alias}` target `{}` must contain exactly one separator",
                raw.target
            ));
        }
        validated.push((
            alias.clone(),
            CapabilityConfig {
                target: raw.target.clone(),
                description: raw.description.clone(),
                input_schema: raw.input_schema.clone(),
                risk: raw.risk.clone(),
                timeout_seconds: raw.timeout_seconds,
                max_output_bytes: raw.max_output_bytes,
                input_schema_camel: None,
            },
        ));
    }
    validated.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(validated)
}
