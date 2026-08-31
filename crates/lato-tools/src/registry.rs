use crate::{ToolKind, ToolSpec};

pub const PHASE0_TOOL_IDS: &[&str] = &[
    "Lato:read_file",
    "Lato:list_dir",
    "Lato:grep",
    "Lato:search_replace",
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
            id: "Lato:run_terminal_command",
            kind: ToolKind::Execute,
        },
        ToolSpec {
            id: "Lato:todo_write",
            kind: ToolKind::Other,
        },
    ]
}

pub fn v1_specs() -> Vec<ToolSpec> {
    let mut specs = phase0_specs();
    specs.extend([
        ToolSpec {
            id: "Lato:spawn_subagent",
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
    }
}
