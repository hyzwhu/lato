//! Progressive discovery helpers for MCP tool search indexes.
//!
//! Used by `lato-tools` `search_tool` / optional direct expansion. Search is
//! purely over the generation-scoped `McpSchemaCache` — it does not start
//! servers or perform `tools/call`.

use crate::registry::{McpSchemaCache, McpToolDescriptor};

/// Soft cap on matches returned to the model from one `search_tool` call.
pub const MAX_SEARCH_MATCHES: usize = 32;

/// One search hit summarized for progressive discovery.
#[derive(Clone, Debug, PartialEq)]
pub struct McpSearchHit {
    pub server: String,
    pub name: String,
    pub qualified_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl From<&McpToolDescriptor> for McpSearchHit {
    fn from(descriptor: &McpToolDescriptor) -> Self {
        Self {
            server: descriptor.server.clone(),
            name: descriptor.name.clone(),
            qualified_name: descriptor.qualified_name.clone(),
            description: descriptor.description.clone(),
            input_schema: descriptor.input_schema.clone(),
        }
    }
}

/// Case-insensitive substring search over qualified name, tool name, server, and description.
pub fn search_tools(cache: &McpSchemaCache, query: &str) -> Vec<McpSearchHit> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return cache
            .iter()
            .take(MAX_SEARCH_MATCHES)
            .map(McpSearchHit::from)
            .collect();
    }
    cache
        .iter()
        .filter(|tool| {
            contains_ci(&tool.qualified_name, &needle)
                || contains_ci(&tool.name, &needle)
                || contains_ci(&tool.server, &needle)
                || contains_ci(&tool.description, &needle)
        })
        .take(MAX_SEARCH_MATCHES)
        .map(McpSearchHit::from)
        .collect()
}

fn contains_ci(haystack: &str, needle_lower: &str) -> bool {
    haystack.to_ascii_lowercase().contains(needle_lower)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seed_cache() -> McpSchemaCache {
        let mut cache = McpSchemaCache::new(1);
        cache.ingest_server_tools(
            "demo",
            "plugin",
            &[
                json!({
                    "name": "echo",
                    "description": "Echo text back",
                    "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
                }),
                json!({
                    "name": "list_items",
                    "description": "Lists catalog items",
                    "inputSchema": {"type": "object"}
                }),
            ],
        );
        cache
    }

    #[test]
    fn search_matches_name_and_description() {
        let cache = seed_cache();
        let by_name = search_tools(&cache, "echo");
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].qualified_name, "demo__echo");
        let by_desc = search_tools(&cache, "catalog");
        assert_eq!(by_desc.len(), 1);
        assert_eq!(by_desc[0].name, "list_items");
        let by_qualified = search_tools(&cache, "demo__list");
        assert_eq!(by_qualified.len(), 1);
    }

    #[test]
    fn empty_query_returns_bounded_prefix() {
        let cache = seed_cache();
        let all = search_tools(&cache, "  ");
        assert_eq!(all.len(), 2);
    }
}
