pub const PROTOCOL_VERSION: u32 = 1;

pub const METHODS_IMPLEMENTED: &[&str] = &[
    "initialize",
    "session/new",
    "session/prompt",
    "session/cancel",
    "session/update",
    "session/request_permission",
    "session/list",
    "session/resume",
    "session/close",
    "session/set_model",
    "lato/session/info",
    "lato/plan/status",
    "lato/plan/enter",
    "lato/plan/submit",
    "lato/plan/approve",
    "lato/plan/exit",
    "lato/session/compact",
    "lato/session/skills",
    "lato/session/skill",
    "lato/session/workflows",
    "lato/session/workflow",
    "lato/session/workflow/runs",
    "lato/session/workflow/pause",
    "lato/session/workflow/resume",
    "lato/session/workflow/stop",
    "lato/session/list",
    "lato/session/rename",
    "lato/session/delete",
    "lato/models/list",
    "lato/auth/login",
    "lato/auth/logout",
    "lato/auth/status",
    "lato/plugins/reload",
];

pub fn is_implemented(method: &str) -> bool {
    METHODS_IMPLEMENTED.contains(&method)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a1_1_initialize_surface() {
        assert!(is_implemented("initialize"));
        assert!(is_implemented("session/resume"));
        assert!(!is_implemented("session/load"));
        assert!(!METHODS_IMPLEMENTED.contains(&"session/load"));
    }

    #[test]
    fn a1_8_load_not_implemented() {
        assert!(!is_implemented("session/load"));
    }

    #[test]
    fn plugin_reload_is_advertised_exactly_once() {
        assert_eq!(
            METHODS_IMPLEMENTED
                .iter()
                .filter(|method| **method == "lato/plugins/reload")
                .count(),
            1
        );
    }

    #[test]
    fn session_workflows_is_advertised() {
        assert!(is_implemented("lato/session/workflows"));
        assert_eq!(
            METHODS_IMPLEMENTED
                .iter()
                .filter(|method| **method == "lato/session/workflows")
                .count(),
            1
        );
    }

    #[test]
    fn workflow_lifecycle_methods_are_advertised_exactly_once() {
        for method in [
            "lato/session/workflow",
            "lato/session/workflow/runs",
            "lato/session/workflow/pause",
            "lato/session/workflow/resume",
            "lato/session/workflow/stop",
        ] {
            assert!(is_implemented(method), "{method} must be advertised");
            assert_eq!(
                METHODS_IMPLEMENTED.iter().filter(|m| **m == method).count(),
                1,
                "{method} must appear exactly once"
            );
        }
        // Out of scope for 7B4: no CLI subcommands, no model-facing tool.
        assert!(!METHODS_IMPLEMENTED.contains(&"lato/workflow/save"));
    }
}
