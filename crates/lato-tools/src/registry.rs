use crate::{ToolKind, ToolSpec};

pub const PHASE0_TOOL_IDS: &[&str] = &[
    "Lato:read_file",
    "Lato:list_dir",
    "Lato:grep",
    "Lato:search_replace",
    "Lato:write_file",
    "Lato:run_terminal_command",
    "Lato:todo_write",
];

pub fn phase0_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            id: "Lato:read_file",
            kind: ToolKind::Read,
        },
        ToolSpec {
            id: "Lato:list_dir",
            kind: ToolKind::ListDir,
        },
        ToolSpec {
            id: "Lato:grep",
            kind: ToolKind::Search,
        },
        ToolSpec {
            id: "Lato:search_replace",
            kind: ToolKind::Edit,
        },
        ToolSpec {
            id: "Lato:write_file",
            kind: ToolKind::Edit,
        },
        ToolSpec {
            id: "Lato:run_terminal_command",
            kind: ToolKind::Execute,
        },
        ToolSpec {
            id: "Lato:todo_write",
            kind: ToolKind::Other,
        },
    ]
}

pub fn phase0_tool_definitions() -> serde_json::Value {
    serde_json::json!([
        {"type":"function","function":{"name":"read_file","description":"Read a UTF-8 file","parameters":{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"required":["path"]}}},
        {"type":"function","function":{"name":"list_dir","description":"List a directory","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}},
        {"type":"function","function":{"name":"grep","description":"Search files with a regular expression","parameters":{"type":"object","properties":{"path":{"type":"string"},"pattern":{"type":"string"}},"required":["path","pattern"]}}},
        {"type":"function","function":{"name":"search_replace","description":"Replace exactly one text occurrence in an existing UTF-8 file. Use write_file to create or overwrite files.","parameters":{"type":"object","properties":{"path":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"}},"required":["path","old","new"]}}},
        {"type":"function","function":{"name":"write_file","description":"Create or overwrite a UTF-8 file in the workspace. Use this to write new files such as hello.go. Prefer this over printing file contents in chat.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"Path relative to the workspace or absolute"},"contents":{"type":"string","description":"Full file contents"}},"required":["path","contents"]}}},
        {"type":"function","function":{"name":"run_terminal_command","description":"Run a command in the workspace shell. Use this for running build/test commands and other shell actions. Use write_file to create files.","parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}},
        {"type":"function","function":{"name":"todo_write","description":"Update the task list","parameters":{"type":"object","properties":{"items":{"type":"array","items":{"type":"string"}}},"required":["items"]}}}
    ])
}

pub fn v1_tool_definitions() -> serde_json::Value {
    let mut definitions = connected_builtin_definitions();
    definitions.extend(crate::task_tool_definitions());
    serde_json::Value::Array(definitions)
}

pub(crate) fn connected_builtin_definitions() -> Vec<serde_json::Value> {
    let mut definitions = phase0_tool_definitions()
        .as_array()
        .cloned()
        .unwrap_or_default();
    definitions.push(serde_json::json!({"type":"function","function":{"name":"web_fetch","description":"Fetch a public HTTP(S) URL with SSRF protection","parameters":{"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}}}));
    definitions
}

pub fn v1_specs() -> Vec<ToolSpec> {
    let mut specs = phase0_specs();
    specs.extend([
        ToolSpec {
            id: "Lato:spawn",
            kind: ToolKind::Other,
        },
        ToolSpec {
            id: "Lato:send",
            kind: ToolKind::Other,
        },
        ToolSpec {
            id: "Lato:wait",
            kind: ToolKind::Other,
        },
        ToolSpec {
            id: "Lato:cancel",
            kind: ToolKind::Other,
        },
        ToolSpec {
            id: "Lato:inspect",
            kind: ToolKind::Other,
        },
        ToolSpec {
            id: "Lato:search_tool",
            kind: ToolKind::Search,
        },
        ToolSpec {
            id: "Lato:use_tool",
            kind: ToolKind::Other,
        },
        ToolSpec {
            id: "Lato:web_search",
            kind: ToolKind::Search,
        },
        ToolSpec {
            id: "Lato:web_fetch",
            kind: ToolKind::Read,
        },
    ]);
    specs
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a2_5_phase0_tool_list() {
        let ids: Vec<_> = phase0_specs().into_iter().map(|s| s.id).collect();
        for forbidden in [
            "Lato:spawn_subagent",
            "Lato:web_search",
            "Lato:web_fetch",
            "Lato:search_tool",
            "Lato:use_tool",
        ] {
            assert!(!ids.contains(&forbidden));
        }
        assert!(ids.contains(&"Lato:read_file"));
        assert!(ids.contains(&"Lato:write_file"));
    }

    #[test]
    fn e5_1_v1_tool_router_adds_only_connected_extended_tools() {
        let definitions = v1_tool_definitions();
        let names: Vec<_> = definitions
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.pointer("/function/name").and_then(|v| v.as_str()))
            .collect();
        assert!(names.contains(&"web_fetch"));
        for name in ["spawn", "send", "wait", "cancel", "inspect"] {
            assert!(names.contains(&name));
        }
        assert!(!names.contains(&"spawn_subagent"));
        assert!(!names.contains(&"search_tool"));
    }
}
