//! MCP server descriptor parsing (stdio + streamable HTTP).
//!
//! Parsing only — this module never starts processes or opens HTTP sessions.

use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use serde_json::Value;
use url::Url;

use crate::names::normalize_server_name;

pub const MAX_SERVERS_PER_PLUGIN: usize = 32;
pub const MAX_ENV_ENTRIES: usize = 64;
pub const MAX_HEADER_ENTRIES: usize = 64;
pub const MAX_ARGS: usize = 64;
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
pub const MAX_MCP_DIAGNOSTICS: usize = 128;
pub const MAX_MCP_DIAGNOSTIC_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpTransportKind {
    Stdio,
    StreamableHttp,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpServerSpec {
    /// Stable id: `{plugin}/{server}`.
    pub id: String,
    pub plugin_name: String,
    pub server_name: String,
    pub transport: McpTransportKind,
    pub command: Option<PathBuf>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub url: Option<Url>,
    pub headers: Vec<(String, String)>,
    pub timeout_ms: u64,
    pub source_dir: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpDiagnostic {
    pub code: String,
    pub plugin_name: String,
    pub path: Option<PathBuf>,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct McpDescriptorSet {
    pub generation: u64,
    pub servers: Arc<[McpServerSpec]>,
    pub diagnostics: Arc<[McpDiagnostic]>,
}

impl McpDescriptorSet {
    pub fn empty(generation: u64) -> Arc<Self> {
        Arc::new(Self {
            generation,
            servers: Arc::from([]),
            diagnostics: Arc::from([]),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ParseContext<'a> {
    pub plugin_name: &'a str,
    pub plugin_root: &'a Path,
    pub source_dir: PathBuf,
    pub source_path: Option<&'a Path>,
}

/// Parse a `.mcp.json` / inline MCP config value into server specs.
///
/// Accepts either `{ "mcpServers": { ... } }` or a bare servers object.
pub fn parse_mcp_config(
    value: &Value,
    context: &ParseContext<'_>,
    diagnostics: &mut Vec<McpDiagnostic>,
) -> Vec<McpServerSpec> {
    let Some(servers) = extract_servers_object(value) else {
        push_diagnostic(
            diagnostics,
            "mcp.config_shape",
            context.plugin_name,
            context.source_path.map(ToOwned::to_owned),
            "MCP configuration must be an object with mcpServers or a servers map",
        );
        return Vec::new();
    };
    let mut out = Vec::new();
    for (raw_name, entry) in servers {
        if out.len() >= MAX_SERVERS_PER_PLUGIN {
            push_diagnostic(
                diagnostics,
                "mcp.servers_limit",
                context.plugin_name,
                context.source_path.map(ToOwned::to_owned),
                &format!("plugin exceeds {MAX_SERVERS_PER_PLUGIN} MCP servers; extras ignored"),
            );
            break;
        }
        match parse_server_entry(raw_name, entry, context) {
            Ok(spec) => out.push(spec),
            Err(message) => push_diagnostic(
                diagnostics,
                "mcp.server_invalid",
                context.plugin_name,
                context.source_path.map(ToOwned::to_owned),
                &message,
            ),
        }
    }
    out
}

fn extract_servers_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    let object = value.as_object()?;
    if let Some(servers) = object.get("mcpServers") {
        return servers.as_object();
    }
    // Bare servers map: every value should be an object (server entry).
    if object.values().all(Value::is_object) {
        return Some(object);
    }
    None
}

fn parse_server_entry(
    raw_name: &str,
    entry: &Value,
    context: &ParseContext<'_>,
) -> Result<McpServerSpec, String> {
    let Some(server_name) = normalize_server_name(raw_name) else {
        return Err(format!("invalid MCP server name {raw_name:?}"));
    };
    let Some(object) = entry.as_object() else {
        return Err(format!("MCP server {server_name} must be an object"));
    };

    let timeout_ms = clamp_timeout(object.get("timeout").and_then(Value::as_u64));
    let env = parse_env(object.get("env"));
    let headers = parse_headers(object.get("headers"))?;
    let args = parse_args(object.get("args"))?;

    let has_command = object
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    let has_url = object
        .get("url")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    let transport_hint = object
        .get("transport")
        .and_then(Value::as_str)
        .map(|value| value.to_ascii_lowercase());

    if has_command && has_url {
        return Err(format!(
            "MCP server {server_name} must not set both command and url"
        ));
    }

    if has_url
        || matches!(
            transport_hint.as_deref(),
            Some("streamable-http" | "streamable_http" | "http")
        )
    {
        let url_raw = object
            .get("url")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("MCP server {server_name} requires url"))?;
        let url = Url::parse(url_raw)
            .map_err(|error| format!("MCP server {server_name} has invalid url: {error}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!(
                "MCP server {server_name} url must be http or https"
            ));
        }
        return Ok(McpServerSpec {
            id: format!("{}/{}", context.plugin_name, server_name),
            plugin_name: context.plugin_name.to_owned(),
            server_name,
            transport: McpTransportKind::StreamableHttp,
            command: None,
            args: Vec::new(),
            env,
            cwd: None,
            url: Some(url),
            headers,
            timeout_ms,
            source_dir: context.source_dir.clone(),
        });
    }

    if !has_command {
        return Err(format!(
            "MCP server {server_name} requires command (stdio) or url (streamable HTTP)"
        ));
    }

    let command_raw = object
        .get("command")
        .and_then(Value::as_str)
        .expect("checked has_command");
    let command = resolve_command(command_raw, context.plugin_root)?;
    let cwd = match object.get("cwd").and_then(Value::as_str) {
        Some(raw) if !raw.is_empty() => Some(resolve_under_plugin(raw, context.plugin_root)?),
        _ => None,
    };

    Ok(McpServerSpec {
        id: format!("{}/{}", context.plugin_name, server_name),
        plugin_name: context.plugin_name.to_owned(),
        server_name,
        transport: McpTransportKind::Stdio,
        command: Some(command),
        args,
        env,
        cwd,
        url: None,
        headers: Vec::new(),
        timeout_ms,
        source_dir: context.source_dir.clone(),
    })
}

fn clamp_timeout(configured: Option<u64>) -> u64 {
    let value = configured.unwrap_or(DEFAULT_TIMEOUT_MS);
    if value == 0 {
        DEFAULT_TIMEOUT_MS
    } else {
        value.min(MAX_TIMEOUT_MS)
    }
}

fn parse_env(value: Option<&Value>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let Some(object) = value.and_then(Value::as_object) else {
        return env;
    };
    for (key, value) in object.iter().take(MAX_ENV_ENTRIES) {
        if reserved_env(key) {
            continue;
        }
        if let Some(value) = value.as_str() {
            env.insert(key.clone(), value.to_owned());
        }
    }
    env
}

fn parse_headers(value: Option<&Value>) -> Result<Vec<(String, String)>, String> {
    let Some(object) = value.and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut headers = Vec::new();
    for (key, value) in object.iter().take(MAX_HEADER_ENTRIES) {
        let Some(value) = value.as_str() else {
            return Err(format!("header {key} must be a string"));
        };
        headers.push((key.clone(), value.to_owned()));
    }
    Ok(headers)
}

fn parse_args(value: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err("args must be an array of strings".into());
    };
    let mut args = Vec::new();
    for item in items.iter().take(MAX_ARGS) {
        let Some(item) = item.as_str() else {
            return Err("args must be an array of strings".into());
        };
        args.push(item.to_owned());
    }
    Ok(args)
}

fn resolve_command(command: &str, plugin_root: &Path) -> Result<PathBuf, String> {
    if is_bare_command(command) {
        return Ok(PathBuf::from(command));
    }
    resolve_under_plugin(command, plugin_root)
}

fn is_bare_command(command: &str) -> bool {
    !command.is_empty()
        && !command.contains('/')
        && !command.contains('\\')
        && !command.starts_with('.')
}

fn resolve_under_plugin(relative: &str, plugin_root: &Path) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!("path {relative:?} escapes the plugin root"));
    }
    let canonical_root = dunce::canonicalize(plugin_root)
        .map_err(|error| format!("failed to canonicalize plugin root: {error}"))?;
    let joined = plugin_root.join(path);
    // Allow non-existent relative targets (e.g. scripts created later) when the
    // lexical join stays under the root; prefer canonicalize when present.
    if let Ok(canonical) = dunce::canonicalize(&joined) {
        if !canonical.starts_with(&canonical_root) {
            return Err(format!("path {relative:?} escapes the plugin root"));
        }
        return Ok(canonical);
    }
    let normalized = canonical_root.join(path);
    if !normalized.starts_with(&canonical_root) {
        return Err(format!("path {relative:?} escapes the plugin root"));
    }
    Ok(normalized)
}

/// Identity / forgery-sensitive keys that plugin MCP env must not override.
pub fn reserved_env(key: &str) -> bool {
    matches!(
        key,
        "LATO_SESSION_ID"
            | "LATO_WORKSPACE_ROOT"
            | "LATO_PLUGIN_NAME"
            | "LATO_MCP_SERVER"
            | "LATO_MCP_SERVER_ID"
            | "CLAUDE_PROJECT_DIR"
    ) || key.starts_with("LATO_MCP_")
}

pub fn push_diagnostic(
    diagnostics: &mut Vec<McpDiagnostic>,
    code: &str,
    plugin_name: &str,
    path: Option<PathBuf>,
    message: &str,
) {
    if diagnostics.len() >= MAX_MCP_DIAGNOSTICS {
        return;
    }
    let mut message = message.to_owned();
    if message.len() > MAX_MCP_DIAGNOSTIC_BYTES {
        let mut end = MAX_MCP_DIAGNOSTIC_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    diagnostics.push(McpDiagnostic {
        code: code.to_owned(),
        plugin_name: plugin_name.to_owned(),
        path,
        message,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn parses_stdio_and_http_shapes() {
        let dir = tempdir().unwrap();
        let ctx = ParseContext {
            plugin_name: "demo",
            plugin_root: dir.path(),
            source_dir: dir.path().to_path_buf(),
            source_path: None,
        };
        let mut diagnostics = Vec::new();
        let specs = parse_mcp_config(
            &json!({
                "mcpServers": {
                    "stdio": {
                        "command": "node",
                        "args": ["server.js"],
                        "env": {"DEMO":"1","LATO_SESSION_ID":"forged"},
                        "timeout": 5
                    },
                    "http": {
                        "url": "https://127.0.0.1:9443/mcp",
                        "headers": {"Authorization":"Bearer x"},
                        "transport": "streamable-http"
                    }
                }
            }),
            &ctx,
            &mut diagnostics,
        );
        assert!(diagnostics.is_empty());
        assert_eq!(specs.len(), 2);
        let stdio = specs.iter().find(|s| s.server_name == "stdio").unwrap();
        assert_eq!(stdio.transport, McpTransportKind::Stdio);
        assert_eq!(stdio.command.as_deref(), Some(Path::new("node")));
        assert_eq!(stdio.args, vec!["server.js"]);
        assert_eq!(stdio.env.get("DEMO").map(String::as_str), Some("1"));
        assert!(!stdio.env.contains_key("LATO_SESSION_ID"));
        assert_eq!(stdio.timeout_ms, 5);
        let http = specs.iter().find(|s| s.server_name == "http").unwrap();
        assert_eq!(http.transport, McpTransportKind::StreamableHttp);
        assert_eq!(http.headers.len(), 1);
    }

    #[test]
    fn rejects_parent_dir_escape() {
        let dir = tempdir().unwrap();
        let ctx = ParseContext {
            plugin_name: "demo",
            plugin_root: dir.path(),
            source_dir: dir.path().to_path_buf(),
            source_path: None,
        };
        let mut diagnostics = Vec::new();
        let specs = parse_mcp_config(
            &json!({
                "mcpServers": {
                    "bad": {"command": "../escape", "cwd": "../outside"}
                }
            }),
            &ctx,
            &mut diagnostics,
        );
        assert!(specs.is_empty());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code == "mcp.server_invalid" && d.message.contains("escapes"))
        );
    }
}
