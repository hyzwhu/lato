use std::{collections::BTreeMap, fs, path::PathBuf, sync::Arc};

use regex::Regex;
use serde_json::Value;

use crate::PluginSnapshot;

use super::HookEventName;

const MAX_PATTERN_BYTES: usize = 1024;
const MAX_SIMPLE_ALTERNATIVES: usize = 64;
const MAX_DIAGNOSTICS: usize = 128;
const MAX_DIAGNOSTIC_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandlerType {
    Command,
    Http,
}

#[derive(Clone, Debug)]
enum MatcherKind {
    All,
    Exact(Vec<String>),
    Regex(Regex),
}

#[derive(Clone, Debug)]
pub struct HookMatcher {
    configured: String,
    compiled: MatcherKind,
}

impl HookMatcher {
    pub fn compile(pattern: &str) -> Result<Self, regex::Error> {
        if pattern.len() > MAX_PATTERN_BYTES {
            return Err(Regex::new("(").unwrap_err());
        }
        let compiled = if pattern.is_empty() || pattern == "*" {
            MatcherKind::All
        } else if pattern
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'|')
        {
            let values = pattern
                .split('|')
                .take(MAX_SIMPLE_ALTERNATIVES + 1)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            if values.len() > MAX_SIMPLE_ALTERNATIVES {
                return Err(Regex::new("(").unwrap_err());
            }
            MatcherKind::Exact(values)
        } else {
            MatcherKind::Regex(Regex::new(pattern)?)
        };
        Ok(Self {
            configured: pattern.to_owned(),
            compiled,
        })
    }

    pub fn configured(&self) -> &str {
        &self.configured
    }

    pub fn matches(&self, value: &str) -> bool {
        match &self.compiled {
            MatcherKind::All => true,
            MatcherKind::Exact(values) => values.iter().any(|candidate| {
                candidate == value
                    || compatibility_aliases(value)
                        .iter()
                        .any(|alias| candidate == alias)
            }),
            MatcherKind::Regex(regex) => {
                regex.is_match(value)
                    || compatibility_aliases(value)
                        .iter()
                        .any(|alias| regex.is_match(alias))
            }
        }
    }
}

fn compatibility_aliases(value: &str) -> Vec<String> {
    let mut aliases = vec![value.to_owned()];
    if let Some(stripped) = value.strip_prefix("mcp__") {
        aliases.push(stripped.replace("__", "_"));
    }
    aliases.push(value.replace('-', "_"));
    aliases.sort();
    aliases.dedup();
    aliases
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookDiagnostic {
    pub code: String,
    pub plugin_name: String,
    pub path: Option<PathBuf>,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct HookSpec {
    pub id: String,
    pub plugin_name: String,
    pub event: HookEventName,
    pub handler_type: HandlerType,
    pub matcher: Option<HookMatcher>,
    pub command: Option<String>,
    pub url: Option<String>,
    pub timeout_ms: u64,
    pub source_dir: PathBuf,
    pub extra_env: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct HookRegistry {
    generation: u64,
    by_event: BTreeMap<HookEventName, Arc<[HookSpec]>>,
    diagnostics: Arc<[HookDiagnostic]>,
}

impl HookRegistry {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn handlers(&self, event: HookEventName) -> &[HookSpec] {
        self.by_event.get(&event).map(AsRef::as_ref).unwrap_or(&[])
    }

    pub fn diagnostics(&self) -> &[HookDiagnostic] {
        &self.diagnostics
    }
}

pub fn materialize_hooks(snapshot: &PluginSnapshot) -> Arc<HookRegistry> {
    let mut by_event: BTreeMap<HookEventName, Vec<HookSpec>> = BTreeMap::new();
    let mut diagnostics = Vec::new();
    let mut plugins = snapshot.active_plugins().collect::<Vec<_>>();
    plugins.sort_by(|a, b| a.name.cmp(&b.name).then(a.canonical_root.cmp(&b.canonical_root)));
    for plugin in plugins {
        let mut sources = Vec::new();
        if let Some(path) = &plugin.hooks_path {
            match fs::read_to_string(path)
                .map_err(|error| error.to_string())
                .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
            {
                Ok(value) => sources.push((Some(path.clone()), value)),
                Err(message) => push_diagnostic(
                    &mut diagnostics,
                    "hook.config_invalid",
                    &plugin.name,
                    Some(path.clone()),
                    &message,
                ),
            }
        }
        if let Some(value) = &plugin.inline_hooks {
            sources.push((None, value.clone()));
        }
        sources.sort_by(|a, b| a.0.cmp(&b.0));
        for (source_index, (path, value)) in sources.into_iter().enumerate() {
            parse_source(
                &plugin.name,
                &plugin.canonical_root,
                path,
                source_index,
                &value,
                &mut by_event,
                &mut diagnostics,
            );
        }
    }
    Arc::new(HookRegistry {
        generation: snapshot.generation(),
        by_event: by_event
            .into_iter()
            .map(|(event, handlers)| (event, handlers.into()))
            .collect(),
        diagnostics: diagnostics.into(),
    })
}

fn parse_source(
    plugin_name: &str,
    plugin_root: &std::path::Path,
    path: Option<PathBuf>,
    source_index: usize,
    value: &Value,
    by_event: &mut BTreeMap<HookEventName, Vec<HookSpec>>,
    diagnostics: &mut Vec<HookDiagnostic>,
) {
    let Some(events) = value.get("hooks").unwrap_or(value).as_object() else {
        push_diagnostic(diagnostics, "hook.config_shape", plugin_name, path, "hook configuration must be an object");
        return;
    };
    for (event_name, groups) in events {
        let Some(event) = HookEventName::parse(event_name) else {
            push_diagnostic(diagnostics, "hook.event_unknown", plugin_name, path.clone(), "unknown hook event");
            continue;
        };
        let Some(groups) = groups.as_array() else {
            push_diagnostic(diagnostics, "hook.groups_invalid", plugin_name, path.clone(), "hook event groups must be an array");
            continue;
        };
        for (group_index, group) in groups.iter().enumerate() {
            let matcher = group.get("matcher").and_then(Value::as_str).unwrap_or("*");
            let matcher = match HookMatcher::compile(matcher) {
                Ok(matcher) => matcher,
                Err(error) => {
                    push_diagnostic(diagnostics, "hook.matcher_invalid", plugin_name, path.clone(), &error.to_string());
                    continue;
                }
            };
            let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
                push_diagnostic(diagnostics, "hook.handlers_invalid", plugin_name, path.clone(), "hook group requires a hooks array");
                continue;
            };
            for (handler_index, handler) in handlers.iter().enumerate() {
                let Some(kind) = handler.get("type").and_then(Value::as_str) else {
                    push_diagnostic(diagnostics, "hook.handler_type_missing", plugin_name, path.clone(), "hook handler type is required");
                    continue;
                };
                let (handler_type, command, url) = match kind.to_ascii_lowercase().as_str() {
                    "command" => {
                        let command = handler.get("command").and_then(Value::as_str).filter(|value| !value.is_empty());
                        let Some(command) = command else {
                            push_diagnostic(diagnostics, "hook.command_missing", plugin_name, path.clone(), "command handler requires command");
                            continue;
                        };
                        (HandlerType::Command, Some(command.to_owned()), None)
                    }
                    "http" | "https" => {
                        let url = handler.get("url").and_then(Value::as_str).filter(|value| !value.is_empty());
                        let Some(url) = url else {
                            push_diagnostic(diagnostics, "hook.url_missing", plugin_name, path.clone(), "HTTP handler requires url");
                            continue;
                        };
                        (HandlerType::Http, None, Some(url.to_owned()))
                    }
                    _ => {
                        push_diagnostic(diagnostics, "hook.handler_type_unknown", plugin_name, path.clone(), "unsupported hook handler type");
                        continue;
                    }
                };
                let configured_timeout = handler.get("timeout").and_then(Value::as_u64).unwrap_or(0);
                let mut timeout_ms = if configured_timeout == 0 { event.default_timeout_ms() } else { configured_timeout.saturating_mul(1000) };
                if event == HookEventName::SessionEnd && configured_timeout > 0 {
                    timeout_ms = timeout_ms.min(60_000);
                }
                let mut extra_env = BTreeMap::new();
                if let Some(env) = handler.get("env").and_then(Value::as_object) {
                    for (key, value) in env {
                        if reserved_env(key) { continue; }
                        if let Some(value) = value.as_str() { extra_env.insert(key.clone(), value.to_owned()); }
                    }
                }
                by_event.entry(event).or_default().push(HookSpec {
                    id: format!("{plugin_name}:{source_index}:{}:{group_index}:{handler_index}", event.as_str()),
                    plugin_name: plugin_name.to_owned(),
                    event,
                    handler_type,
                    matcher: Some(matcher.clone()),
                    command,
                    url,
                    timeout_ms,
                    source_dir: path.as_ref().and_then(|path| path.parent()).unwrap_or(plugin_root).to_path_buf(),
                    extra_env,
                });
            }
        }
    }
}

fn reserved_env(key: &str) -> bool {
    matches!(key, "LATO_HOOK_EVENT" | "LATO_HOOK_NAME" | "LATO_SESSION_ID" | "LATO_WORKSPACE_ROOT" | "CLAUDE_PROJECT_DIR")
}

fn push_diagnostic(diagnostics: &mut Vec<HookDiagnostic>, code: &str, plugin_name: &str, path: Option<PathBuf>, message: &str) {
    if diagnostics.len() >= MAX_DIAGNOSTICS { return; }
    let mut message = message.to_owned();
    if message.len() > MAX_DIAGNOSTIC_BYTES {
        let mut end = MAX_DIAGNOSTIC_BYTES;
        while !message.is_char_boundary(end) { end -= 1; }
        message.truncate(end);
    }
    diagnostics.push(HookDiagnostic { code: code.to_owned(), plugin_name: plugin_name.to_owned(), path, message });
}
