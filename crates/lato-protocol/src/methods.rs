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
    "lato/models/list",
    "lato/auth/login",
    "lato/auth/logout",
    "lato/auth/status",
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
}
