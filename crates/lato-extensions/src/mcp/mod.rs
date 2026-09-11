// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138 plugin MCP descriptor patterns
// License: Apache-2.0
// Lato changes: materializes immutable descriptors from Phase 6A PluginSnapshot only; never starts processes

//! Materialize trusted+enabled MCP server descriptors from a frozen [`PluginSnapshot`].

use std::{collections::BTreeSet, fs, sync::Arc};

use lato_mcp::{
    McpDescriptorSet, McpDiagnostic, McpServerSpec, ParseContext, parse_mcp_config, push_diagnostic,
};
use serde_json::Value;

use crate::PluginSnapshot;

/// Build an immutable MCP descriptor set for the snapshot generation.
///
/// Consumes only [`PluginSnapshot::active_plugins`] (trusted ∧ enabled).
/// Does not start MCP processes or open HTTP clients.
pub fn materialize_mcp(snapshot: &PluginSnapshot) -> Arc<McpDescriptorSet> {
    let mut servers: Vec<McpServerSpec> = Vec::new();
    let mut diagnostics: Vec<McpDiagnostic> = Vec::new();
    // Collision policy: keep first registration of a normalized server_name;
    // diagnose and skip later collisions. Plugins are ordered by name then root.
    let mut seen_server_names: BTreeSet<String> = BTreeSet::new();

    let mut plugins = snapshot.active_plugins().collect::<Vec<_>>();
    plugins.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.canonical_root.cmp(&b.canonical_root))
    });

    for plugin in plugins {
        let mut sources = Vec::new();
        if let Some(path) = &plugin.mcp_config_path {
            match fs::read_to_string(path)
                .map_err(|error| error.to_string())
                .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|e| e.to_string()))
            {
                Ok(value) => sources.push((Some(path.clone()), value)),
                Err(message) => push_diagnostic(
                    &mut diagnostics,
                    "mcp.config_invalid",
                    &plugin.name,
                    Some(path.clone()),
                    &message,
                ),
            }
        }
        if let Some(value) = &plugin.inline_mcp_servers {
            // Inline mcpServers field is the servers map (or wrapped object).
            let wrapped = match value {
                Value::Object(map) if map.contains_key("mcpServers") || map.values().all(Value::is_object) => {
                    value.clone()
                }
                other => Value::Object(
                    [("mcpServers".to_owned(), other.clone())]
                        .into_iter()
                        .collect(),
                ),
            };
            sources.push((None, wrapped));
        }
        sources.sort_by(|a, b| a.0.cmp(&b.0));

        for (path, value) in sources {
            let source_dir = path
                .as_ref()
                .and_then(|path| path.parent())
                .unwrap_or(plugin.canonical_root.as_path())
                .to_path_buf();
            let context = ParseContext {
                plugin_name: &plugin.name,
                plugin_root: &plugin.canonical_root,
                source_dir,
                source_path: path.as_deref(),
            };
            let parsed = parse_mcp_config(&value, &context, &mut diagnostics);
            for spec in parsed {
                if !seen_server_names.insert(spec.server_name.clone()) {
                    push_diagnostic(
                        &mut diagnostics,
                        "mcp.server_collision",
                        &plugin.name,
                        path.clone(),
                        &format!(
                            "MCP server name {:?} collides with an earlier registration; keeping the first",
                            spec.server_name
                        ),
                    );
                    continue;
                }
                servers.push(spec);
            }
        }
    }

    let set = Arc::new(McpDescriptorSet {
        generation: snapshot.generation(),
        servers: servers.into(),
        diagnostics: diagnostics.into(),
        allowed_tools: None,
    });
    // Apply the snapshot's monotone MCP ceiling (server + qualified-tool allowlists).
    let ceiling = snapshot.mcp_ceiling();
    set.narrow(ceiling.allowed_servers.as_ref(), ceiling.allowed_tools.as_ref())
}

pub use lato_mcp::{
    DEFAULT_TIMEOUT_MS, MAX_ENV_ENTRIES, MAX_HEADER_ENTRIES, MAX_SERVERS_PER_PLUGIN, MAX_TIMEOUT_MS,
    McpTransportKind, qualify_tool,
};
