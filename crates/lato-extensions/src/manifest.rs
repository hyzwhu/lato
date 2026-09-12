// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-agent/src/plugins/manifest.rs
// License: Apache-2.0
// Lato changes: accepts only root plugin.json, adds convention results and strict fail-closed path limits

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{MAX_COMPONENT_PATH_BYTES, MAX_COMPONENT_PATHS, MAX_PLUGIN_NAME_LEN};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Author {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum PathOrPaths {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum PathOrInline {
    Path(String),
    Inline(serde_json::Value),
}

/// Unknown fields intentionally remain accepted for forward compatibility.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<Author>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub skills: Option<PathOrPaths>,
    #[serde(default)]
    pub hooks: Option<PathOrInline>,
    #[serde(default)]
    pub mcp_servers: Option<PathOrInline>,
    #[serde(default)]
    pub workflows: Option<PathOrInline>,
}

impl PluginManifest {
    pub fn validate(&self) -> Result<(), ManifestError> {
        if valid_plugin_name(&self.name) {
            return Ok(());
        }
        Err(ManifestError::InvalidName {
            name: self.name.clone(),
            reason: format!(
                "must be 1-{MAX_PLUGIN_NAME_LEN} chars, lowercase alphanumeric + hyphens, no leading/trailing hyphens"
            ),
        })
    }

    pub fn skill_dirs(&self, plugin_root: &Path) -> Vec<PathBuf> {
        match &self.skills {
            Some(paths) => paths
                .values()
                .take(MAX_COMPONENT_PATHS)
                .filter_map(|path| resolve_existing(plugin_root, path, ExpectedKind::Directory))
                .collect(),
            None => resolve_existing(plugin_root, "skills", ExpectedKind::Directory)
                .into_iter()
                .collect(),
        }
    }

    pub fn hooks_path(&self, plugin_root: &Path) -> Option<PathBuf> {
        resolve_component(&self.hooks, plugin_root, "hooks/hooks.json")
    }

    pub fn mcp_config_path(&self, plugin_root: &Path) -> Option<PathBuf> {
        resolve_component(&self.mcp_servers, plugin_root, ".mcp.json")
    }

    pub fn inline_hooks(&self) -> Option<&serde_json::Value> {
        inline_value(&self.hooks)
    }

    pub fn inline_mcp_servers(&self) -> Option<&serde_json::Value> {
        inline_value(&self.mcp_servers)
    }

    pub fn workflow_config_path(&self, plugin_root: &Path) -> Option<PathBuf> {
        resolve_component(&self.workflows, plugin_root, "workflows.json")
    }

    pub fn inline_workflows(&self) -> Option<&serde_json::Value> {
        inline_value(&self.workflows)
    }
}

impl PathOrPaths {
    fn values(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        match self {
            Self::Single(path) => Box::new(std::iter::once(path.as_str())),
            Self::Multiple(paths) => Box::new(paths.iter().map(String::as_str)),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ManifestLoadResult {
    Found(Box<PluginManifest>),
    Convention(Box<PluginManifest>),
    NotFound,
}

pub fn load_manifest(plugin_root: &Path) -> Result<ManifestLoadResult, ManifestError> {
    let manifest_path = plugin_root.join("plugin.json");
    if manifest_path.is_file() {
        let content =
            std::fs::read_to_string(&manifest_path).map_err(|source| ManifestError::IoError {
                path: manifest_path.clone(),
                source,
            })?;
        let manifest = serde_json::from_str::<PluginManifest>(&content).map_err(|error| {
            ManifestError::ParseError {
                path: manifest_path,
                message: error.to_string(),
            }
        })?;
        manifest.validate()?;
        return Ok(ManifestLoadResult::Found(Box::new(manifest)));
    }

    let Some(name) = name_from_dirname(plugin_root) else {
        return Ok(ManifestLoadResult::NotFound);
    };
    let manifest = PluginManifest {
        name,
        ..PluginManifest::default()
    };
    if !manifest.skill_dirs(plugin_root).is_empty()
        || manifest.hooks_path(plugin_root).is_some()
        || manifest.mcp_config_path(plugin_root).is_some()
        || manifest.workflow_config_path(plugin_root).is_some()
    {
        Ok(ManifestLoadResult::Convention(Box::new(manifest)))
    } else {
        Ok(ManifestLoadResult::NotFound)
    }
}

pub fn name_from_dirname(dir: &Path) -> Option<String> {
    let dirname = dir.file_name()?.to_str()?;
    let sanitized = dirname
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('-').to_owned();
    valid_plugin_name(&trimmed).then_some(trimmed)
}

fn valid_plugin_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PLUGIN_NAME_LEN
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

fn resolve_component(
    field: &Option<PathOrInline>,
    plugin_root: &Path,
    convention: &str,
) -> Option<PathBuf> {
    match field {
        Some(PathOrInline::Path(path)) => resolve_existing(plugin_root, path, ExpectedKind::File),
        Some(PathOrInline::Inline(_)) => None,
        None => resolve_existing(plugin_root, convention, ExpectedKind::File),
    }
}

fn inline_value(field: &Option<PathOrInline>) -> Option<&serde_json::Value> {
    match field {
        Some(PathOrInline::Inline(value)) => Some(value),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum ExpectedKind {
    Directory,
    File,
}

fn resolve_existing(plugin_root: &Path, relative: &str, kind: ExpectedKind) -> Option<PathBuf> {
    if relative.is_empty() || relative.len() > MAX_COMPONENT_PATH_BYTES {
        return None;
    }
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return None;
    }
    let canonical_root = dunce::canonicalize(plugin_root).ok()?;
    let canonical_path = dunce::canonicalize(plugin_root.join(relative)).ok()?;
    if !canonical_path.starts_with(&canonical_root) {
        return None;
    }
    match kind {
        ExpectedKind::Directory if canonical_path.is_dir() => Some(canonical_path),
        ExpectedKind::File if canonical_path.is_file() => Some(canonical_path),
        _ => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("invalid plugin name {name:?}: {reason}")]
    InvalidName { name: String, reason: String },
    #[error("failed to read {path}: {source}")]
    IoError {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {message}")]
    ParseError { path: PathBuf, message: String },
}
