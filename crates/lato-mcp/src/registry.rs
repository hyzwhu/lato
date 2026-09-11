//! Generation-scoped MCP tool schema cache and discovery helpers.
//!
//! Derived from: Lato MCP naming/collision policy (`names.rs`) and Phase 6C
//! product design §8 (tools/list → stable cache; bad schema isolation).
//! License: Apache-2.0 (workspace)
//! Lato changes: snapshot-generation schema cache with diagnose+keep-first
//! collision policy; no ToolRuntime / tools/call bypass.

use std::{collections::BTreeMap, sync::Arc};

use serde_json::{Map, Value};

use crate::{
    config::{McpDiagnostic, push_diagnostic},
    error::McpError,
    names::qualify_tool,
};

/// Soft cap on tools retained per server for one generation.
pub const MAX_TOOLS_PER_SERVER: usize = 256;
/// Soft cap on `tools/list` pagination rounds.
pub const MAX_TOOLS_LIST_PAGES: usize = 16;
pub const MAX_TOOL_NAME_LEN: usize = 128;
pub const MAX_TOOL_DESCRIPTION_BYTES: usize = 4_096;

/// Cached descriptor for one MCP tool discovered via `tools/list`.
#[derive(Clone, Debug, PartialEq)]
pub struct McpToolDescriptor {
    pub server: String,
    pub name: String,
    pub qualified_name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Immutable-within-generation cache of discovered MCP tool schemas.
///
/// # Collision policy
///
/// **Diagnose + keep first / reject later.** When a newly discovered tool would
/// claim a `qualified_name` (`server__tool`) already present in this cache, the
/// first registration is retained and the later one is rejected with an
/// `mcp.tool_collision` diagnostic. Never silently overwrite. The same policy
/// applies to duplicate tool names inside a single `tools/list` response.
#[derive(Clone, Debug, Default)]
pub struct McpSchemaCache {
    generation: u64,
    by_qualified: BTreeMap<String, McpToolDescriptor>,
    by_server: BTreeMap<String, Arc<[McpToolDescriptor]>>,
    diagnostics: Vec<McpDiagnostic>,
}

impl McpSchemaCache {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            by_qualified: BTreeMap::new(),
            by_server: BTreeMap::new(),
            diagnostics: Vec::new(),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.by_qualified.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_qualified.is_empty()
    }

    pub fn diagnostics(&self) -> &[McpDiagnostic] {
        &self.diagnostics
    }

    pub fn tools_for_server(&self, server: &str) -> Option<&[McpToolDescriptor]> {
        self.by_server.get(server).map(|tools| tools.as_ref())
    }

    pub fn iter(&self) -> impl Iterator<Item = &McpToolDescriptor> {
        self.by_qualified.values()
    }

    /// Lookup by qualified wire name (`server__tool`) or `server/tool` parts.
    pub fn lookup(&self, qualified_or_parts: &str) -> Option<&McpToolDescriptor> {
        if let Some(found) = self.by_qualified.get(qualified_or_parts) {
            return Some(found);
        }
        if let Some((server, tool)) = split_server_tool_parts(qualified_or_parts) {
            let qualified = qualify_tool(server, tool);
            return self.by_qualified.get(&qualified);
        }
        None
    }

    /// Ingest tools discovered for `server`, applying collision + bad-schema isolation.
    ///
    /// Returns the number of tools successfully retained for this server.
    pub fn ingest_server_tools(
        &mut self,
        server: &str,
        plugin_name: &str,
        tools: &[Value],
    ) -> usize {
        let mut retained = Vec::new();
        for (index, tool_value) in tools.iter().enumerate() {
            if retained.len() >= MAX_TOOLS_PER_SERVER {
                push_diagnostic(
                    &mut self.diagnostics,
                    "mcp.tools_limit",
                    plugin_name,
                    None,
                    &format!(
                        "server `{server}` exceeded {MAX_TOOLS_PER_SERVER} tools; extras ignored"
                    ),
                );
                break;
            }
            match parse_tool_descriptor(server, tool_value) {
                Ok(descriptor) => {
                    if self.by_qualified.contains_key(&descriptor.qualified_name)
                        || retained.iter().any(|existing: &McpToolDescriptor| {
                            existing.qualified_name == descriptor.qualified_name
                        })
                    {
                        push_diagnostic(
                            &mut self.diagnostics,
                            "mcp.tool_collision",
                            plugin_name,
                            None,
                            &format!(
                                "keeping first tool `{}`; rejecting later registration (index {index})",
                                descriptor.qualified_name
                            ),
                        );
                        continue;
                    }
                    retained.push(descriptor);
                }
                Err(message) => {
                    push_diagnostic(
                        &mut self.diagnostics,
                        "mcp.tool_schema_invalid",
                        plugin_name,
                        None,
                        &format!("server `{server}` tool index {index}: {message}"),
                    );
                }
            }
        }

        for descriptor in &retained {
            self.by_qualified
                .insert(descriptor.qualified_name.clone(), descriptor.clone());
        }
        let count = retained.len();
        self.by_server
            .insert(server.to_owned(), Arc::from(retained));
        count
    }

    pub fn has_server(&self, server: &str) -> bool {
        self.by_server.contains_key(server)
    }
}

/// Parse a `tools/list` JSON-RPC result object into tool values + optional cursor.
pub fn parse_tools_list_result(result: Value) -> Result<(Vec<Value>, Option<String>), McpError> {
    let object = result
        .as_object()
        .ok_or_else(|| McpError::protocol("tools/list result must be an object"))?;
    let tools = match object.get("tools") {
        None => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => {
            return Err(McpError::protocol(
                "tools/list result.tools must be an array",
            ));
        }
    };
    let next_cursor = object
        .get("nextCursor")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|cursor| !cursor.is_empty());
    Ok((tools, next_cursor))
}

pub fn tools_list_params(cursor: Option<&str>) -> Value {
    match cursor {
        Some(cursor) => serde_json::json!({ "cursor": cursor }),
        None => Value::Object(Map::new()),
    }
}

fn parse_tool_descriptor(server: &str, value: &Value) -> Result<McpToolDescriptor, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "tool entry must be an object".to_owned())?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "tool name missing or empty".to_owned())?;
    if name.len() > MAX_TOOL_NAME_LEN {
        return Err(format!("tool name exceeds {MAX_TOOL_NAME_LEN} bytes"));
    }
    if !tool_name_allowed(name) {
        return Err(format!("tool name `{name}` has disallowed characters"));
    }
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let description = truncate_description(description);
    let input_schema = match object.get("inputSchema") {
        Some(schema) if schema.is_object() => schema.clone(),
        Some(_) => return Err("inputSchema must be a JSON object".to_owned()),
        None => return Err("inputSchema missing".to_owned()),
    };
    Ok(McpToolDescriptor {
        server: server.to_owned(),
        name: name.to_owned(),
        qualified_name: qualify_tool(server, name),
        description,
        input_schema,
    })
}

fn tool_name_allowed(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphanumeric() || first == '_') {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.')
}

fn truncate_description(mut description: String) -> String {
    if description.len() <= MAX_TOOL_DESCRIPTION_BYTES {
        return description;
    }
    let mut end = MAX_TOOL_DESCRIPTION_BYTES;
    while !description.is_char_boundary(end) {
        end -= 1;
    }
    description.truncate(end);
    description
}

fn split_server_tool_parts(raw: &str) -> Option<(&str, &str)> {
    // Prefer explicit `server/tool` parts form (not used for wire names).
    let (server, tool) = raw.split_once('/')?;
    if server.is_empty() || tool.is_empty() || tool.contains('/') {
        return None;
    }
    Some((server, tool))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keeps_first_on_collision_and_isolates_bad_schema() {
        let mut cache = McpSchemaCache::new(7);
        let tools = vec![
            json!({
                "name": "alpha",
                "description": "first",
                "inputSchema": {"type": "object"}
            }),
            json!({
                "name": "alpha",
                "description": "duplicate",
                "inputSchema": {"type": "object"}
            }),
            json!({
                "name": "",
                "inputSchema": {"type": "object"}
            }),
            json!({
                "name": "beta",
                "description": "ok",
                "inputSchema": "not-an-object"
            }),
            json!({
                "name": "gamma",
                "description": "kept",
                "inputSchema": {"type": "object", "properties": {}}
            }),
        ];
        let retained = cache.ingest_server_tools("demo", "plugin", &tools);
        assert_eq!(retained, 2);
        assert_eq!(cache.len(), 2);
        let alpha = cache.lookup("demo__alpha").unwrap();
        assert_eq!(alpha.description, "first");
        assert!(cache.lookup("demo__gamma").is_some());
        assert!(cache.lookup("demo/gamma").is_some());
        let codes: Vec<_> = cache
            .diagnostics()
            .iter()
            .map(|d| d.code.as_str())
            .collect();
        assert!(codes.contains(&"mcp.tool_collision"));
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == "mcp.tool_schema_invalid")
                .count(),
            2
        );
    }

    #[test]
    fn parse_tools_list_reads_cursor() {
        let (tools, cursor) = parse_tools_list_result(json!({
            "tools": [{"name": "a"}],
            "nextCursor": "page-2"
        }))
        .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(cursor.as_deref(), Some("page-2"));
    }
}
