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

/// One reloadable configuration source (Round-3 tri-state contract):
/// - `Ok(None)` — the source legitimately contributes nothing (file absent,
///   no `agentfield` stanza, or stanza explicitly disabled);
/// - `Ok(Some(config))` — the source is present, valid, and enabled;
/// - `Err(CatalogSourceError)` — the source is PRESENT but unusable
///   (unreadable, malformed JSON, invalid stanza). An invalid source is
///   never silently treated as absent: assembly fails closed, so
///   registration sees zero registration and the post-approval recheck
///   returns `agentfield.catalog_changed`.
#[derive(Debug, Clone)]
pub struct CatalogSourceError(pub String);

pub type CatalogSource =
    std::sync::Arc<dyn Fn() -> Result<Option<AgentFieldConfig>, CatalogSourceError> + Send + Sync>;

fn load_config_file(
    path: &std::path::Path,
) -> Result<Option<AgentFieldConfig>, CatalogSourceError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(CatalogSourceError(format!(
                "config file {} is present but unreadable: {error}",
                path.display()
            )));
        }
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        CatalogSourceError(format!(
            "config file {} is present but malformed JSON: {error}",
            path.display()
        ))
    })?;
    let Some(raw) = value.get("agentfield") else {
        // No agentfield stanza: the file is a valid config that simply does
        // not configure this adapter.
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    match crate::agentfield::config::AgentFieldConfig::parse(raw) {
        Ok(Some(config)) if config.enabled => Ok(Some(config)),
        // Disabled stanzas are validated but contribute nothing; the
        // enabled flip is detectable because an enabled stanza assembles
        // differently.
        Ok(_) => Ok(None),
        Err(error) => Err(CatalogSourceError(format!(
            "config file {} has an invalid agentfield stanza: {error}",
            path.display()
        ))),
    }
}

/// The default source set: user, project, plugin (in merge order).
pub fn catalog_sources(home: &std::path::Path, cwd: &std::path::Path) -> Vec<CatalogSource> {
    let user_config = home.to_path_buf();
    let project_config = cwd.join(".lato").join("config.json");
    let plugin_home = home.to_path_buf();
    let plugin_cwd = cwd.to_path_buf();
    vec![
        std::sync::Arc::new(move || load_config_file(&user_config.join("config.json"))),
        std::sync::Arc::new(move || load_config_file(&project_config)),
        // Plugin source (Round-3 production wiring): real, filesystem-
        // backed, and observable — plugin additions, edits, removals, and
        // malformed manifests are detected at registration and recheck.
        std::sync::Arc::new(move || load_plugin_contribution(&plugin_home, &plugin_cwd)),
    ]
}

/// Every discovered plugin manifest path: `<cwd>/.lato/plugins/*/plugin.json`
/// and `<lato_home>/plugins/*/plugin.json`, sorted for deterministic merge
/// order (the same roots the plugin system scans).
fn plugin_manifest_paths(home: &std::path::Path, cwd: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for root in [cwd.join(".lato").join("plugins"), home.join("plugins")] {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let manifest = entry.path().join("plugin.json");
            if manifest.is_file() {
                paths.push(manifest);
            }
        }
    }
    paths.sort();
    paths
}

/// Merge every plugin manifest's optional `agentfield` stanza into one
/// plugin contribution. A manifest that is present but unreadable/malformed,
/// an invalid stanza, or two plugins disagreeing on origin / credential
/// reference / the same capability alias all fail closed (`Err`). Manifests
/// without an `agentfield` stanza and disabled stanzas contribute nothing.
fn load_plugin_contribution(
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> Result<Option<AgentFieldConfig>, CatalogSourceError> {
    let mut assembled: Option<AgentFieldConfig> = None;
    for manifest in plugin_manifest_paths(home, cwd) {
        let bytes = std::fs::read(&manifest).map_err(|error| {
            CatalogSourceError(format!(
                "plugin manifest {} is present but unreadable: {error}",
                manifest.display()
            ))
        })?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
            CatalogSourceError(format!(
                "plugin manifest {} is present but malformed JSON: {error}",
                manifest.display()
            ))
        })?;
        let Some(raw) = value.get("agentfield") else {
            continue;
        };
        if raw.is_null() {
            continue;
        }
        let config = crate::agentfield::config::AgentFieldConfig::parse(raw).map_err(|error| {
            CatalogSourceError(format!(
                "plugin manifest {} has an invalid agentfield stanza: {error}",
                manifest.display()
            ))
        })?;
        let Some(config) = config.filter(|config| config.enabled) else {
            continue;
        };
        match assembled.as_mut() {
            None => assembled = Some(config),
            Some(current) => {
                if current.origin != config.origin
                    || current.credential_reference != config.credential_reference
                {
                    return Err(CatalogSourceError(
                        "plugin manifests disagree on origin or credential reference".to_owned(),
                    ));
                }
                for (alias, capability) in config.capabilities {
                    if let Some((_, existing)) = current
                        .capabilities
                        .iter()
                        .find(|(existing_alias, _)| *existing_alias == alias)
                    {
                        if *existing != capability {
                            return Err(CatalogSourceError(format!(
                                "plugin manifests disagree on capability '{alias}'"
                            )));
                        }
                    } else {
                        current.capabilities.push((alias, capability));
                    }
                }
            }
        }
    }
    Ok(assembled)
}

/// Deterministically merge the present sources into the assembled config:
/// every present source must agree on origin and credential reference
/// (disagreement fails closed), capabilities are unioned by alias (a
/// conflicting redefinition fails closed), and the merged capability set is
/// still capped at [`config::MAX_CAPABILITIES`]. `None` = nothing present,
/// a disagreement, or ANY source that is present but invalid (fail closed).
pub fn assemble_catalog_config(sources: &[CatalogSource]) -> Option<AgentFieldConfig> {
    let mut assembled: Option<AgentFieldConfig> = None;
    for source in sources {
        let config = match source() {
            Ok(Some(config)) => config,
            Ok(None) => continue,
            // Present-but-invalid source: never silently skipped. The
            // message carries only a path and a parse error — no secrets —
            // and `lato doctor` reports the same diagnostics.
            Err(CatalogSourceError(_message)) => return None,
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

    // ---- Round-3 tri-state source semantics --------------------------------

    fn write_config(path: &std::path::Path, value: serde_json::Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value.to_string()).unwrap();
    }

    fn enabled_capability() -> serde_json::Value {
        json!({"agentfield": {
            "enabled": true,
            "baseUrl": "https://agents.example.internal",
            "credential": "agentfield:primary",
            "capabilities": single_capability(),
        }})
    }

    #[test]
    fn absent_sources_contribute_nothing_and_enabled_source_assembles() {
        let temp = tempfile::TempDir::new().unwrap();
        let sources = catalog_sources(temp.path(), temp.path());
        // Nothing present anywhere: nothing to assemble (no registration).
        assert!(assemble_catalog_config(&sources).is_none());

        // A valid enabled user config assembles.
        write_config(&temp.path().join("config.json"), enabled_capability());
        assert!(assemble_catalog_config(&sources).is_some());
    }

    #[test]
    fn disabled_stanza_is_legitimately_empty_but_flip_is_detectable() {
        let temp = tempfile::TempDir::new().unwrap();
        let sources = catalog_sources(temp.path(), temp.path());
        let mut disabled = enabled_capability();
        disabled["agentfield"]["enabled"] = json!(false);
        write_config(&temp.path().join("config.json"), disabled);
        assert!(
            assemble_catalog_config(&sources).is_none(),
            "disabled contributes nothing"
        );
        // Flipping the same stanza to enabled changes the assembled config.
        write_config(&temp.path().join("config.json"), enabled_capability());
        assert!(assemble_catalog_config(&sources).is_some());
    }

    #[test]
    fn malformed_json_source_fails_closed() {
        let temp = tempfile::TempDir::new().unwrap();
        let sources = catalog_sources(temp.path(), temp.path());
        std::fs::create_dir_all(temp.path().join(".lato")).unwrap();
        std::fs::write(temp.path().join(".lato").join("config.json"), "{not json").unwrap();
        assert!(
            assemble_catalog_config(&sources).is_none(),
            "present-but-malformed must never be treated as absent"
        );
    }

    #[test]
    fn invalid_stanza_source_fails_closed() {
        let temp = tempfile::TempDir::new().unwrap();
        let sources = catalog_sources(temp.path(), temp.path());
        // Valid JSON, but the stanza is not a valid agentfield config.
        write_config(
            &temp.path().join("config.json"),
            json!({"agentfield": {"enabled": true}}),
        );
        assert!(assemble_catalog_config(&sources).is_none());
    }

    #[test]
    fn unreadable_source_fails_closed() {
        let temp = tempfile::TempDir::new().unwrap();
        let sources = catalog_sources(temp.path(), temp.path());
        // A directory at the config path is readable-neither: fs::read
        // fails with a non-NotFound error, which must fail closed.
        std::fs::create_dir_all(temp.path().join("config.json")).unwrap();
        assert!(assemble_catalog_config(&sources).is_none());
    }

    #[test]
    fn unchanged_multi_source_assembly_is_stable() {
        let temp = tempfile::TempDir::new().unwrap();
        write_config(&temp.path().join("config.json"), enabled_capability());
        write_config(
            &temp.path().join(".lato").join("config.json"),
            enabled_capability(),
        );
        let sources = catalog_sources(temp.path(), temp.path());
        // Same origin+credential, identical capability: the union is
        // deterministic and repeated assemblies agree.
        let first = assemble_catalog_config(&sources).unwrap();
        let second = assemble_catalog_config(&sources).unwrap();
        assert_eq!(
            AgentFieldCatalog::from_config(&first).revision(),
            AgentFieldCatalog::from_config(&second).revision(),
            "unchanged sources must re-assemble to the same revision"
        );
    }

    // ---- Round-4 production plugin source ----------------------------------

    fn write_plugin(
        home: &std::path::Path,
        name: &str,
        agentfield: Option<serde_json::Value>,
    ) -> std::path::PathBuf {
        let dir = home.join("plugins").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = json!({"name": name});
        if let Some(agentfield) = agentfield {
            manifest["agentfield"] = agentfield;
        }
        let path = dir.join("plugin.json");
        std::fs::write(&path, manifest.to_string()).unwrap();
        path
    }

    #[test]
    fn plugin_contribution_is_observed_by_the_production_source() {
        let temp = tempfile::TempDir::new().unwrap();
        let sources = catalog_sources(temp.path(), temp.path());
        assert!(
            assemble_catalog_config(&sources).is_none(),
            "no plugins yet"
        );

        write_plugin(
            temp.path(),
            "alpha",
            Some(enabled_capability()["agentfield"].clone()),
        );
        let with_plugin = assemble_catalog_config(&sources).expect("plugin contributes");
        assert!(
            with_plugin
                .capabilities
                .iter()
                .any(|(alias, _)| alias == "contract-review"),
            "plugin capability enters the assembly"
        );

        // A manifest without the stanza contributes nothing.
        write_plugin(temp.path(), "beta", None);
        let again = assemble_catalog_config(&sources).unwrap();
        assert_eq!(
            AgentFieldCatalog::from_config(&with_plugin).revision(),
            AgentFieldCatalog::from_config(&again).revision()
        );
    }

    #[test]
    fn malformed_plugin_manifest_fails_closed() {
        let temp = tempfile::TempDir::new().unwrap();
        let sources = catalog_sources(temp.path(), temp.path());
        let dir = temp.path().join("plugins").join("alpha");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), "{not json").unwrap();
        assert!(assemble_catalog_config(&sources).is_none());
    }

    #[test]
    fn disagreeing_plugin_origins_fail_closed() {
        let temp = tempfile::TempDir::new().unwrap();
        let sources = catalog_sources(temp.path(), temp.path());
        write_plugin(
            temp.path(),
            "alpha",
            Some(enabled_capability()["agentfield"].clone()),
        );
        let mut other = enabled_capability()["agentfield"].clone();
        other["baseUrl"] = json!("https://other.example.internal");
        other["capabilities"] = json!({});
        write_plugin(temp.path(), "beta", Some(other));
        assert!(assemble_catalog_config(&sources).is_none());
    }

    #[test]
    fn plugin_capability_conflict_fails_closed() {
        let temp = tempfile::TempDir::new().unwrap();
        let sources = catalog_sources(temp.path(), temp.path());
        write_plugin(
            temp.path(),
            "alpha",
            Some(enabled_capability()["agentfield"].clone()),
        );
        let mut conflicting = enabled_capability()["agentfield"].clone();
        conflicting["capabilities"] = json!({
            "contract-review": {
                "target": "legal.review_other",
                "description": "Conflicting redefinition",
                "inputSchema": {"type":"object"},
                "risk": "remote_read",
            }
        });
        write_plugin(temp.path(), "beta", Some(conflicting));
        assert!(assemble_catalog_config(&sources).is_none());
    }
}
