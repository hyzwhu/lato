// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-agent/src/plugins/discovery.rs
// License: Apache-2.0
// Lato changes: scans only CLI, project .lato, and LATO_HOME sources with bounded diagnostics

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    MAX_DIAGNOSTIC_MESSAGE_BYTES, MAX_DISCOVERED_PLUGINS, MAX_DISCOVERY_DIAGNOSTICS,
    ManifestLoadResult, PluginManifest, load_manifest, trust::source_is_trusted,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginScope {
    CliOverride = 0,
    Project = 1,
    User = 2,
}

impl PluginScope {
    pub fn id_label(self) -> &'static str {
        match self {
            Self::CliOverride => "cli",
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

impl std::fmt::Display for PluginScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.id_label())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginOrigin {
    CliOverride,
    Project,
    User,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct PluginId(pub String);

impl PluginId {
    pub fn new(scope: PluginScope, canonical_root: &Path, name: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(canonical_root.to_string_lossy().as_bytes());
        let hash = hasher.finalize();
        Self(format!(
            "{}/{:02x}{:02x}{:02x}{:02x}/{name}",
            scope.id_label(),
            hash[0],
            hash[1],
            hash[2],
            hash[3]
        ))
    }
}

impl std::fmt::Display for PluginId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DiscoveredPlugin {
    pub manifest: PluginManifest,
    pub id: PluginId,
    pub root: PathBuf,
    pub canonical_root: PathBuf,
    pub scope: PluginScope,
    pub origin: PluginOrigin,
    pub trusted: bool,
    pub skill_dirs: Vec<PathBuf>,
    pub hooks_path: Option<PathBuf>,
    pub mcp_config_path: Option<PathBuf>,
    pub conflict: Option<String>,
}

impl DiscoveredPlugin {
    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    pub fn plugin_name(&self) -> &str {
        self.name()
    }
}

#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    pub cwd: PathBuf,
    pub lato_home: PathBuf,
    pub cli_plugin_dirs: Vec<PathBuf>,
    pub project_trusted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct DiscoveryResult {
    pub plugins: Vec<DiscoveredPlugin>,
    pub diagnostics: Vec<DiscoveryDiagnostic>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiscoveryDiagnostic {
    pub code: String,
    pub message: String,
    pub scope: Option<PluginScope>,
    pub path: Option<String>,
}

pub fn discover_plugins(config: &DiscoveryConfig) -> DiscoveryResult {
    let mut result = DiscoveryResult::default();
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();

    let mut cli_roots = config.cli_plugin_dirs.clone();
    cli_roots.sort();
    for root in cli_roots {
        collect_candidate(
            &root,
            PluginScope::CliOverride,
            PluginOrigin::CliOverride,
            config.project_trusted,
            &mut seen,
            &mut candidates,
            &mut result.diagnostics,
        );
    }

    scan_parent(
        &config.cwd.join(".lato/plugins"),
        PluginScope::Project,
        PluginOrigin::Project,
        config.project_trusted,
        &mut seen,
        &mut candidates,
        &mut result.diagnostics,
    );
    scan_parent(
        &config.lato_home.join("plugins"),
        PluginScope::User,
        PluginOrigin::User,
        config.project_trusted,
        &mut seen,
        &mut candidates,
        &mut result.diagnostics,
    );

    candidates.sort_by(|left, right| {
        left.scope
            .cmp(&right.scope)
            .then_with(|| left.canonical_root.cmp(&right.canonical_root))
    });
    let mut winners = HashMap::<String, usize>::new();
    for candidate in candidates {
        if let Some(winner_index) = winners.get(candidate.name()).copied() {
            let message = format!(
                "plugin name {} from {} at {} was ignored; {} at {} has precedence",
                candidate.name(),
                candidate.scope,
                candidate.canonical_root.display(),
                result.plugins[winner_index].scope,
                result.plugins[winner_index].canonical_root.display()
            );
            result.plugins[winner_index].conflict = Some(bounded(&message));
            push_diagnostic(
                &mut result.diagnostics,
                "plugin.name_conflict",
                &message,
                Some(candidate.scope),
                Some(&candidate.canonical_root),
            );
            continue;
        }
        winners.insert(candidate.name().to_owned(), result.plugins.len());
        result.plugins.push(candidate);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn scan_parent(
    parent: &Path,
    scope: PluginScope,
    origin: PluginOrigin,
    project_trusted: bool,
    seen: &mut HashSet<PathBuf>,
    candidates: &mut Vec<DiscoveredPlugin>,
    diagnostics: &mut Vec<DiscoveryDiagnostic>,
) {
    if !parent.exists() {
        return;
    }
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) => {
            push_diagnostic(
                diagnostics,
                "plugin.source_unreadable",
                &format!("cannot read plugin source {}: {error}", parent.display()),
                Some(scope),
                Some(parent),
            );
            return;
        }
    };
    let mut roots = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect::<Vec<_>>();
    roots.sort();
    for root in roots {
        if root.is_dir() || root.is_symlink() {
            collect_candidate(
                &root,
                scope,
                origin,
                project_trusted,
                seen,
                candidates,
                diagnostics,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_candidate(
    root: &Path,
    scope: PluginScope,
    origin: PluginOrigin,
    project_trusted: bool,
    seen: &mut HashSet<PathBuf>,
    candidates: &mut Vec<DiscoveredPlugin>,
    diagnostics: &mut Vec<DiscoveryDiagnostic>,
) {
    if candidates.len() >= MAX_DISCOVERED_PLUGINS {
        push_diagnostic(
            diagnostics,
            "plugin.candidate_limit",
            "plugin candidate limit reached; remaining candidates were ignored",
            Some(scope),
            Some(root),
        );
        return;
    }
    let canonical_root = match dunce::canonicalize(root) {
        Ok(path) if path.is_dir() => path,
        Ok(_) => {
            push_diagnostic(
                diagnostics,
                "plugin.root_not_directory",
                &format!("plugin root is not a directory: {}", root.display()),
                Some(scope),
                Some(root),
            );
            return;
        }
        Err(error) => {
            push_diagnostic(
                diagnostics,
                "plugin.root_unreadable",
                &format!("cannot resolve plugin root {}: {error}", root.display()),
                Some(scope),
                Some(root),
            );
            return;
        }
    };
    if !seen.insert(canonical_root.clone()) {
        push_diagnostic(
            diagnostics,
            "plugin.duplicate_root",
            &format!(
                "plugin root {} resolves to an already discovered directory",
                root.display()
            ),
            Some(scope),
            Some(root),
        );
        return;
    }
    let manifest = match load_manifest(&canonical_root) {
        Ok(ManifestLoadResult::Found(manifest) | ManifestLoadResult::Convention(manifest)) => {
            *manifest
        }
        Ok(ManifestLoadResult::NotFound) => return,
        Err(error) => {
            push_diagnostic(
                diagnostics,
                "plugin.manifest_invalid",
                &error.to_string(),
                Some(scope),
                Some(&canonical_root),
            );
            return;
        }
    };
    let skill_dirs = manifest.skill_dirs(&canonical_root);
    let hooks_path = manifest.hooks_path(&canonical_root);
    let mcp_config_path = manifest.mcp_config_path(&canonical_root);
    let name = manifest.name.clone();
    candidates.push(DiscoveredPlugin {
        manifest,
        id: PluginId::new(scope, &canonical_root, &name),
        root: root.to_path_buf(),
        canonical_root,
        scope,
        origin,
        trusted: source_is_trusted(scope, project_trusted),
        skill_dirs,
        hooks_path,
        mcp_config_path,
        conflict: None,
    });
}

fn push_diagnostic(
    diagnostics: &mut Vec<DiscoveryDiagnostic>,
    code: &str,
    message: &str,
    scope: Option<PluginScope>,
    path: Option<&Path>,
) {
    if diagnostics.len() >= MAX_DISCOVERY_DIAGNOSTICS {
        return;
    }
    diagnostics.push(DiscoveryDiagnostic {
        code: code.to_owned(),
        message: bounded(message),
        scope,
        path: path.map(|path| bounded(&path.to_string_lossy())),
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
