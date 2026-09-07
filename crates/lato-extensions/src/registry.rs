// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-agent/src/plugins/registry.rs
// License: Apache-2.0
// Lato changes: publishes immutable generation snapshots and derives monotone child capability views

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use lato_core::ToolCapability;
use serde::{Deserialize, Serialize};

use crate::{
    DiscoveredPlugin, DiscoveryDiagnostic, DiscoveryResult, MAX_DIAGNOSTIC_MESSAGE_BYTES,
    MAX_DISCOVERY_DIAGNOSTICS, PathOrInline, PluginId, PluginOrigin, PluginScope,
};

const MAX_CONFIG_ENTRIES: usize = 1024;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PluginConfig {
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default)]
    pub disabled: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginComponentKind {
    Skills,
    Hooks,
    Mcp,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadedPlugin {
    pub name: String,
    pub id: PluginId,
    pub root: PathBuf,
    pub canonical_root: PathBuf,
    pub scope: PluginScope,
    pub origin: PluginOrigin,
    pub trusted: bool,
    pub enabled: bool,
    pub active: bool,
    pub version: Option<String>,
    pub description: Option<String>,
    pub skill_dirs: Vec<PathBuf>,
    pub hooks_path: Option<PathBuf>,
    pub mcp_config_path: Option<PathBuf>,
    pub inline_hooks: Option<serde_json::Value>,
    pub inline_mcp_servers: Option<serde_json::Value>,
    pub conflict: Option<String>,
}

impl LoadedPlugin {
    pub fn components(&self) -> impl Iterator<Item = PluginComponentKind> + '_ {
        let skills = (!self.skill_dirs.is_empty()).then_some(PluginComponentKind::Skills);
        let hooks = (self.hooks_path.is_some() || self.inline_hooks.is_some())
            .then_some(PluginComponentKind::Hooks);
        let mcp = (self.mcp_config_path.is_some() || self.inline_mcp_servers.is_some())
            .then_some(PluginComponentKind::Mcp);
        skills.into_iter().chain(hooks).chain(mcp)
    }
}

#[derive(Clone, Debug)]
pub struct PluginSnapshot {
    generation: u64,
    parent_generation: Option<u64>,
    built_at_ms: u64,
    project_trusted: bool,
    cli_plugin_dirs: Arc<[PathBuf]>,
    plugins: Arc<[LoadedPlugin]>,
    diagnostics: Arc<[DiscoveryDiagnostic]>,
}

impl PluginSnapshot {
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            generation: 0,
            parent_generation: None,
            built_at_ms: 0,
            project_trusted: false,
            cli_plugin_dirs: Arc::from([]),
            plugins: Arc::from([]),
            diagnostics: Arc::from([]),
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn parent_generation(&self) -> Option<u64> {
        self.parent_generation
    }

    pub fn built_at_ms(&self) -> u64 {
        self.built_at_ms
    }

    pub fn project_trusted(&self) -> bool {
        self.project_trusted
    }

    pub fn cli_plugin_dirs(&self) -> &[PathBuf] {
        &self.cli_plugin_dirs
    }

    pub fn plugins(&self) -> &[LoadedPlugin] {
        &self.plugins
    }

    pub fn diagnostics(&self) -> &[DiscoveryDiagnostic] {
        &self.diagnostics
    }

    pub fn active_plugins(&self) -> impl Iterator<Item = &LoadedPlugin> {
        self.plugins.iter().filter(|plugin| plugin.active)
    }

    pub fn active_names(&self) -> Vec<&str> {
        self.active_plugins()
            .map(|plugin| plugin.name.as_str())
            .collect()
    }

    pub fn derive_child(&self, ceiling: &CapabilityCeiling) -> Arc<Self> {
        let extension_allowed = [
            ceiling.parent.as_slice(),
            ceiling.profile.as_slice(),
            ceiling.workspace.as_slice(),
        ]
        .into_iter()
        .all(|capabilities| capabilities.contains(&ToolCapability::ExtensionInvoke));
        let plugins = self
            .plugins
            .iter()
            .cloned()
            .map(|mut plugin| {
                if !extension_allowed {
                    plugin.active = false;
                    plugin.skill_dirs.clear();
                    plugin.hooks_path = None;
                    plugin.mcp_config_path = None;
                    plugin.inline_hooks = None;
                    plugin.inline_mcp_servers = None;
                }
                plugin
            })
            .collect::<Vec<_>>();
        Arc::new(Self {
            generation: self.generation,
            parent_generation: Some(self.generation),
            built_at_ms: self.built_at_ms,
            project_trusted: self.project_trusted,
            cli_plugin_dirs: Arc::clone(&self.cli_plugin_dirs),
            plugins: plugins.into(),
            diagnostics: Arc::clone(&self.diagnostics),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct CapabilityCeiling {
    pub parent: Vec<ToolCapability>,
    pub profile: Vec<ToolCapability>,
    pub workspace: Vec<ToolCapability>,
}

pub fn build_snapshot(
    generation: u64,
    discovery: DiscoveryResult,
    config: &PluginConfig,
) -> Result<Arc<PluginSnapshot>, RegistryBuildError> {
    if generation == 0 {
        return Err(RegistryBuildError::InvalidGeneration);
    }
    let built_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| RegistryBuildError::Clock)?
        .as_millis()
        .try_into()
        .map_err(|_| RegistryBuildError::Clock)?;
    let enabled = config
        .enabled
        .iter()
        .take(MAX_CONFIG_ENTRIES)
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let disabled = config
        .disabled
        .iter()
        .take(MAX_CONFIG_ENTRIES)
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let known = discovery
        .plugins
        .iter()
        .flat_map(|plugin| [plugin.name(), plugin.id.0.as_str()])
        .collect::<HashSet<_>>();
    let mut diagnostics = discovery.diagnostics;
    for configured in config
        .enabled
        .iter()
        .chain(&config.disabled)
        .take(MAX_CONFIG_ENTRIES)
        .filter(|configured| !known.contains(configured.as_str()))
    {
        push_diagnostic(
            &mut diagnostics,
            "plugin.config_unknown",
            &format!("plugin configuration references unknown plugin {configured:?}"),
        );
    }
    let plugins = discovery
        .plugins
        .into_iter()
        .map(|plugin| load_plugin(plugin, &enabled, &disabled, &mut diagnostics))
        .collect::<Vec<_>>();
    Ok(Arc::new(PluginSnapshot {
        generation,
        parent_generation: None,
        built_at_ms,
        project_trusted: discovery.project_trusted,
        cli_plugin_dirs: discovery.cli_plugin_dirs.into(),
        plugins: plugins.into(),
        diagnostics: diagnostics.into(),
    }))
}

fn load_plugin(
    plugin: DiscoveredPlugin,
    enabled: &HashSet<&str>,
    disabled: &HashSet<&str>,
    diagnostics: &mut Vec<DiscoveryDiagnostic>,
) -> LoadedPlugin {
    let explicitly_enabled =
        enabled.contains(plugin.name()) || enabled.contains(plugin.id.0.as_str());
    let explicitly_disabled =
        disabled.contains(plugin.name()) || disabled.contains(plugin.id.0.as_str());
    let default_enabled = plugin.scope == PluginScope::CliOverride;
    let is_enabled = !explicitly_disabled && (explicitly_enabled || default_enabled);
    if explicitly_enabled && explicitly_disabled {
        push_diagnostic(
            diagnostics,
            "plugin.config_conflict",
            &format!(
                "plugin {:?} is both enabled and disabled; disabled takes precedence",
                plugin.name()
            ),
        );
    }
    let inline_hooks = inline(&plugin.manifest.hooks);
    let inline_mcp_servers = inline(&plugin.manifest.mcp_servers);
    LoadedPlugin {
        name: plugin.manifest.name,
        id: plugin.id,
        root: plugin.root,
        canonical_root: plugin.canonical_root,
        scope: plugin.scope,
        origin: plugin.origin,
        trusted: plugin.trusted,
        enabled: is_enabled,
        active: plugin.trusted && is_enabled,
        version: plugin.manifest.version,
        description: plugin.manifest.description,
        skill_dirs: plugin.skill_dirs,
        hooks_path: plugin.hooks_path,
        mcp_config_path: plugin.mcp_config_path,
        inline_hooks,
        inline_mcp_servers,
        conflict: plugin.conflict,
    }
}

fn inline(field: &Option<PathOrInline>) -> Option<serde_json::Value> {
    match field {
        Some(PathOrInline::Inline(value)) => Some(value.clone()),
        _ => None,
    }
}

fn push_diagnostic(diagnostics: &mut Vec<DiscoveryDiagnostic>, code: &str, message: &str) {
    if diagnostics.len() >= MAX_DISCOVERY_DIAGNOSTICS {
        return;
    }
    diagnostics.push(DiscoveryDiagnostic {
        code: code.to_owned(),
        message: bounded(message),
        scope: None,
        path: None,
    });
}

fn bounded(value: &str) -> String {
    if value.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_DIAGNOSTIC_MESSAGE_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryBuildError {
    #[error("plugin snapshot generation must be greater than zero")]
    InvalidGeneration,
    #[error("system clock cannot represent plugin snapshot build time")]
    Clock,
}
