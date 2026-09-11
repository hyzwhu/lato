//! MCP server/tool naming helpers.
//!
//! Collision policy for descriptors: when two servers normalize to the same
//! `server_name`, keep the first registration (plugins sorted by name then
//! canonical root; file source before inline) and diagnose later ones. Never
//! silently overwrite.

/// Double-underscore wire qualification used by progressive discovery and direct expansion.
pub fn qualify_tool(server: &str, tool: &str) -> String {
    format!("{server}__{tool}")
}

/// Normalize a configured server name to `[a-z0-9][a-z0-9_-]*`.
///
/// Returns `None` when the name is empty or contains disallowed characters
/// after trimming and lowercasing.
pub fn normalize_server_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_SERVER_NAME_LEN {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let mut chars = lower.chars();
    let Some(first) = chars.next() else {
        return None;
    };
    if !first.is_ascii_alphanumeric() {
        return None;
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
        return None;
    }
    Some(lower)
}

pub const MAX_SERVER_NAME_LEN: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualifies_with_double_underscore() {
        assert_eq!(qualify_tool("demo", "list"), "demo__list");
    }

    #[test]
    fn normalizes_and_rejects_invalid_names() {
        assert_eq!(normalize_server_name("Demo_Server").as_deref(), Some("demo_server"));
        assert_eq!(normalize_server_name("a").as_deref(), Some("a"));
        assert!(normalize_server_name("").is_none());
        assert!(normalize_server_name("-leading").is_none());
        assert!(normalize_server_name("has space").is_none());
        assert!(normalize_server_name("UPPER/slash").is_none());
    }
}
