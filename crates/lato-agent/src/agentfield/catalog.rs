// Phase 7C2: session-frozen AgentField catalog revision (spec §6.2).
//
// The local allowlist (`AgentFieldConfig.capabilities`) is the single
// source of truth for what the model may see and start. The revision is
// the SHA-256 of the canonical JSON envelope
//
//   {"adapter":"agentfield-v0.1.138","origin":"<scheme://host:port>",
//    "capabilities":[{alias,target,description,inputSchema,risk,
//                     timeoutSeconds,maxOutputBytes}...]}
//
// with UTF-8 bytes, recursively lexicographically sorted object keys,
// capabilities sorted by alias, decimal integers, and no whitespace.
// `inputSchema` is normalized by the same canonicalization (the outer
// pass normalizes nested values). `start` carries the revision listed by
// `list`; a mismatch after approval is a stable
// `agentfield.catalog_changed` with zero remote requests.

use crate::agentfield::config::{AgentFieldConfig, PINNED_AGENTFIELD_VERSION};
use lato_policy::canonical_arguments;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The frozen canonical adapter identifier (spec §6.2).
fn adapter_id() -> String {
    format!("agentfield-{PINNED_AGENTFIELD_VERSION}")
}

/// Effective-port origin rendering used by the revision digest:
/// `scheme://lowercase-host:effective-port` (default ports applied).
pub fn origin_string(origin: &crate::agentfield::config::ControlPlaneOrigin) -> String {
    let scheme = origin.base.scheme();
    let host = origin.base.host_str().unwrap_or("");
    let effective_port = origin.base.port().unwrap_or(match scheme {
        "https" => 443,
        "http" => 80,
        other => panic!("validated config never carries scheme `{other}`"),
    });
    format!("{scheme}://{host}:{effective_port}")
}

/// Session-frozen catalog snapshot: alias-sorted capability entries plus
/// the revision digest. Built once per session from the validated config;
/// the model-visible `list` and the `start` revision recheck both read it.
#[derive(Clone, Debug)]
pub struct AgentFieldCatalog {
    revision: String,
    origin: String,
    /// Alias → capability, sorted by alias (inherited from config order).
    entries: Vec<(String, crate::agentfield::config::CapabilityConfig)>,
}

impl AgentFieldCatalog {
    /// Freeze a catalog from the validated configuration. The capability
    /// count is already capped by config validation (≤ 64); this re-check
    /// is defense-in-depth.
    pub fn from_config(config: &AgentFieldConfig) -> Self {
        assert!(
            config.capabilities.len() <= crate::agentfield::config::MAX_CAPABILITIES,
            "config validation caps capabilities at MAX_CAPABILITIES"
        );
        let origin = origin_string(&config.origin);
        let entries = config.capabilities.clone();
        let revision = Self::compute_revision(&origin, &entries);
        Self {
            revision,
            origin,
            entries,
        }
    }

    /// `sha256:<64 lowercase hex>` (spec §6.2 / §7.1 output shape).
    pub fn revision(&self) -> &str {
        &self.revision
    }

    /// The hashed origin rendering; never carries credentials or paths.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn capabilities(&self) -> &[(String, crate::agentfield::config::CapabilityConfig)] {
        &self.entries
    }

    pub fn capability(&self, alias: &str) -> Option<&crate::agentfield::config::CapabilityConfig> {
        self.entries
            .iter()
            .find(|(existing, _)| existing == alias)
            .map(|(_, capability)| capability)
    }

    /// Constant-time revision comparison for the post-approval recheck.
    pub fn matches_revision(&self, revision: &str) -> bool {
        let expected = self.revision.as_bytes();
        let provided = revision.as_bytes();
        if expected.len() != provided.len() {
            return false;
        }
        let mut diff = 0u8;
        for (x, y) in expected.iter().zip(provided.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }

    fn compute_revision(
        origin: &str,
        entries: &[(String, crate::agentfield::config::CapabilityConfig)],
    ) -> String {
        let capabilities: Vec<Value> = entries
            .iter()
            .map(|(alias, capability)| {
                json!({
                    "alias": alias,
                    "target": capability.target,
                    "description": capability.description,
                    "inputSchema": capability.input_schema,
                    "risk": capability.risk,
                    "timeoutSeconds": capability.timeout_seconds,
                    "maxOutputBytes": capability.max_output_bytes,
                })
            })
            .collect();
        let envelope = json!({
            "adapter": adapter_id(),
            "origin": origin,
            "capabilities": capabilities,
        });
        // canonical_arguments: recursive key sort + compact serialization,
        // so the nested inputSchema is canonicalized by the same pass.
        let canonical = canonical_arguments(&envelope)
            .expect("catalog envelope is JSON-serializable by construction");
        let digest = Sha256::digest(&canonical);
        let mut encoded = String::with_capacity("sha256:".len() + digest.len() * 2);
        encoded.push_str("sha256:");
        for byte in digest {
            use std::fmt::Write as _;
            write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
        }
        encoded
    }
}

// ---- Configuration sources (Round-1 acceptance P1-3) ----------------------
//
// The frozen catalog is assembled from ordered real configuration sources:
// the user config (`$LATO_HOME/config.json`), the project config
// (`<cwd>/.lato/config.json`, same `agentfield` stanza shape), and a
// reserved plugin slot (v1: always `None`; the wiring point for later
// stages). Sources that are present but disabled contribute nothing, so an
// `enabled` flip is detectable by the post-approval revision recheck.

/// One reloadable configuration source. `None` means the source currently
/// contributes nothing (absent, disabled, or invalid — the doctor reports
/// the details; the adapter fails closed).
pub type CatalogSource = std::sync::Arc<dyn Fn() -> Option<AgentFieldConfig> + Send + Sync>;

fn load_config_file(path: &std::path::Path) -> Option<AgentFieldConfig> {
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let raw = value.get("agentfield")?;
    crate::agentfield::config::AgentFieldConfig::parse(raw)
        .ok()
        .flatten()
        .filter(|config| config.enabled)
}

/// The default source set: user, project, plugin (in merge order).
pub fn catalog_sources(home: &std::path::Path, cwd: &std::path::Path) -> Vec<CatalogSource> {
    let user_config = home.to_path_buf();
    let project_config = cwd.join(".lato").join("config.json");
    vec![
        std::sync::Arc::new(move || load_config_file(&user_config.join("config.json"))),
        std::sync::Arc::new(move || load_config_file(&project_config)),
        // Plugin slot: no plugin-provided agentfield configuration exists in
        // v1; reserved so a source change can be detected fail-closed.
        std::sync::Arc::new(|| None),
    ]
}

/// Deterministically merge the present sources into the assembled config:
/// every present source must agree on origin and credential reference
/// (disagreement fails closed), capabilities are unioned by alias (a
/// conflicting redefinition fails closed), and the merged capability set is
/// still capped at [`config::MAX_CAPABILITIES`]. `None` = nothing present
/// or a disagreement.
pub fn assemble_catalog_config(sources: &[CatalogSource]) -> Option<AgentFieldConfig> {
    let mut assembled: Option<AgentFieldConfig> = None;
    for source in sources {
        let Some(config) = source() else {
            continue;
        };
        let Some(current) = assembled.as_mut() else {
            assembled = Some(config);
            continue;
        };
        if current.origin != config.origin
            || current.credential_reference != config.credential_reference
        {
            return None;
        }
        for (alias, capability) in config.capabilities {
            if let Some((_, existing)) = current
                .capabilities
                .iter()
                .find(|(existing_alias, _)| *existing_alias == alias)
            {
                if *existing != capability {
                    return None;
                }
            } else {
                current.capabilities.push((alias, capability));
            }
        }
    }
    let mut assembled = assembled?;
    if assembled.capabilities.len() > crate::agentfield::config::MAX_CAPABILITIES {
        return None;
    }
    assembled.capabilities.sort_by(|a, b| a.0.cmp(&b.0));
    Some(assembled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn config_with(capabilities: Value) -> AgentFieldConfig {
        let raw = json!({
            "enabled": true,
            "baseUrl": "https://Agents.Example.Internal",
            "credential": "agentfield:primary",
            "capabilities": capabilities,
        });
        AgentFieldConfig::parse(&raw)
            .expect("valid config")
            .expect("enabled config")
    }

    fn single_capability() -> Value {
        json!({
            "contract-review": {
                "target": "legal.review_contract",
                "description": "Review one contract",
                "inputSchema": {"type":"object","additionalProperties":false},
                "risk": "remote_read",
            }
        })
    }

    #[test]
    fn revision_is_sha256_prefixed_and_stable() {
        let catalog = AgentFieldCatalog::from_config(&config_with(single_capability()));
        let revision = catalog.revision();
        assert_eq!(revision.len(), "sha256:".len() + 64);
        let hex = revision.strip_prefix("sha256:").unwrap();
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(
            AgentFieldCatalog::from_config(&config_with(single_capability())).revision(),
            revision
        );
    }

    #[test]
    fn origin_is_normalized_to_lowercase_host_and_effective_port() {
        let catalog = AgentFieldCatalog::from_config(&config_with(single_capability()));
        assert_eq!(catalog.origin(), "https://agents.example.internal:443");
    }

    #[test]
    fn any_capability_field_change_changes_the_revision() {
        let base = AgentFieldCatalog::from_config(&config_with(single_capability()));
        let mutations = [
            json!({
                "contract-review": {
                    "target": "legal.review_other",
                    "description": "Review one contract",
                    "inputSchema": {"type":"object","additionalProperties":false},
                    "risk": "remote_read",
                }
            }),
            json!({
                "contract-review": {
                    "target": "legal.review_contract",
                    "description": "Review two contracts",
                    "inputSchema": {"type":"object","additionalProperties":false},
                    "risk": "remote_read",
                }
            }),
            json!({
                "contract-review": {
                    "target": "legal.review_contract",
                    "description": "Review one contract",
                    "inputSchema": {"type":"object","additionalProperties":true},
                    "risk": "remote_read",
                }
            }),
            json!({
                "contract-review": {
                    "target": "legal.review_contract",
                    "description": "Review one contract",
                    "inputSchema": {"type":"object","additionalProperties":false},
                    "risk": "remote_write",
                }
            }),
            json!({
                "contract-review": {
                    "target": "legal.review_contract",
                    "description": "Review one contract",
                    "inputSchema": {"type":"object","additionalProperties":false},
                    "risk": "remote_read",
                    "timeoutSeconds": 800,
                }
            }),
            json!({
                "contract-review": {
                    "target": "legal.review_contract",
                    "description": "Review one contract",
                    "inputSchema": {"type":"object","additionalProperties":false},
                    "risk": "remote_read",
                    "maxOutputBytes": 1024,
                }
            }),
        ];
        for mutated in mutations {
            let catalog = AgentFieldCatalog::from_config(&config_with(mutated));
            assert_ne!(
                catalog.revision(),
                base.revision(),
                "mutation must change the revision"
            );
        }
    }

    #[test]
    fn alias_set_change_and_order_independence() {
        let base = AgentFieldCatalog::from_config(&config_with(single_capability()));
        let mut two = single_capability();
        two["alpha-task"] = json!({
            "target": "alpha.task",
            "description": "Alpha",
            "inputSchema": {"type":"object"},
            "risk": "remote_read",
        });
        let extended = AgentFieldCatalog::from_config(&config_with(two.clone()));
        assert_ne!(extended.revision(), base.revision());
        // Config parse sorts by alias, so insertion order cannot matter.
        let reordered = json!({
            "alpha-task": two["alpha-task"].clone(),
            "contract-review": two["contract-review"].clone(),
        });
        assert_eq!(
            AgentFieldCatalog::from_config(&config_with(reordered)).revision(),
            extended.revision()
        );
    }

    #[test]
    fn capability_lookup_is_exact_and_list_is_sorted() {
        let mut raw = BTreeMap::new();
        raw.insert(
            "zeta",
            json!({"target":"z.task","description":"Z","inputSchema":{"type":"object"},"risk":"remote_read"}),
        );
        raw.insert(
            "alpha",
            json!({"target":"a.task","description":"A","inputSchema":{"type":"object"},"risk":"remote_read"}),
        );
        let capabilities: Value = serde_json::to_value(&raw).unwrap();
        let catalog = AgentFieldCatalog::from_config(&config_with(capabilities));
        let aliases: Vec<&str> = catalog
            .capabilities()
            .iter()
            .map(|(alias, _)| alias.as_str())
            .collect();
        assert_eq!(aliases, vec!["alpha", "zeta"]);
        assert!(catalog.capability("alpha").is_some());
        assert!(catalog.capability("ALPHA").is_none());
        assert!(catalog.capability("missing").is_none());
    }

    #[test]
    fn revision_match_is_constant_time_and_exact() {
        let catalog = AgentFieldCatalog::from_config(&config_with(single_capability()));
        let revision = catalog.revision().to_owned();
        assert!(catalog.matches_revision(&revision));
        assert!(!catalog.matches_revision(&format!("{revision}0")));
        assert!(!catalog.matches_revision(&format!("sha256:{}", "0".repeat(64))));
        assert!(!catalog.matches_revision(""));
    }

    #[test]
    fn canonical_envelope_has_sorted_keys_and_no_whitespace() {
        // The digest input must be the spec's canonical byte form: no
        // whitespace, recursively sorted keys. Rebuild it and inspect bytes.
        let config = config_with(single_capability());
        let origin = origin_string(&config.origin);
        let capabilities: Vec<Value> = config
            .capabilities
            .iter()
            .map(|(alias, capability)| {
                json!({
                    "alias": alias,
                    "target": capability.target,
                    "description": capability.description,
                    "inputSchema": capability.input_schema,
                    "risk": capability.risk,
                    "timeoutSeconds": capability.timeout_seconds,
                    "maxOutputBytes": capability.max_output_bytes,
                })
            })
            .collect();
        let envelope = json!({
            "adapter": adapter_id(),
            "origin": origin,
            "capabilities": capabilities,
        });
        let canonical = canonical_arguments(&envelope).unwrap();
        let text = String::from_utf8(canonical).unwrap();
        // Compact form: no whitespace around structural tokens (string
        // values may legitimately contain spaces). Top-level keys are
        // sorted: adapter < capabilities < origin.
        assert!(text.starts_with("{\"adapter\":\"agentfield-v0.1.138\",\"capabilities\":"));
        assert!(text.contains("}],\"origin\":\""));
        // Recursively sorted keys: the nested inputSchema serializes with
        // `additionalProperties` before `type`.
        assert!(text.contains("{\"additionalProperties\":false,\"type\":\"object\"}"));
    }
}
