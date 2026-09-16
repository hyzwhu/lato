//! AgentField v1.2.1 boundary guards (spec §0.2 item 7, §0.3, §0.5).
//!
//! 7C1 is an OFFLINE-ONLY foundation: no `agentfield` model tool is
//! registered in any enabled/disabled/unconfigured state, and the module
//! ships no production outbound network path (that is Phase 7C1.1, spec
//! §0.3/§0.4). These source-level guards fail the build if either boundary
//! regresses.

/// 7C1 must not reference AgentField from any tool registration / runtime
/// wiring surface (AC-01).
#[test]
fn agentfield_is_absent_from_every_tool_wiring_surface() {
    let surfaces: &[(&str, &str)] = &[
        ("src/host.rs", include_str!("../src/host.rs")),
        (
            "src/subagent/runner.rs",
            include_str!("../src/subagent/runner.rs"),
        ),
        ("src/skills.rs", include_str!("../src/skills.rs")),
        ("src/journal.rs", include_str!("../src/journal.rs")),
        (
            "src/workflow/tool.rs",
            include_str!("../src/workflow/tool.rs"),
        ),
        ("src/actor.rs", include_str!("../src/actor.rs")),
    ];
    for (path, source) in surfaces {
        assert!(
            !source.to_ascii_lowercase().contains("agentfield"),
            "{path} must not reference agentfield in Phase 7C1; \
             model tool registration belongs to 7C2"
        );
    }
}

/// v1.2.1 DG-01/DG-02: the AgentField module must contain no production
/// outbound network path — no reqwest, no real transport, no DNS or address
/// policy, no public network seam. Production transport and network
/// boundaries belong to Phase 7C1.1 (spec §0.3/§0.4).
#[test]
fn agentfield_module_has_no_production_outbound_network_path() {
    let sources: &[(&str, &str)] = &[
        ("mod.rs", include_str!("../src/agentfield/mod.rs")),
        ("config.rs", include_str!("../src/agentfield/config.rs")),
        ("client.rs", include_str!("../src/agentfield/client.rs")),
        ("types.rs", include_str!("../src/agentfield/types.rs")),
        ("probe.rs", include_str!("../src/agentfield/probe.rs")),
    ];
    let forbidden = [
        "reqwest",
        "ReqwestTransport",
        "connect_with_resolver",
        "with_pinned_addrs",
        "lato_mcp",
        "McpDnsResolver",
        "validate_mcp_url",
        "resolve_to_addrs",
        "lookup_host",
        "TcpStream",
    ];
    for (name, source) in sources {
        for marker in forbidden {
            assert!(
                !source.contains(marker),
                "agentfield/{name} must not reference `{marker}` in v1.2.1: \
                 the offline foundation ships no production outbound path"
            );
        }
    }
}

/// The module entry point must stay the v1.2.1 five sub-modules, and the
/// module must not gain tool-protocol dependencies (`lato_core::Tool` /
/// `lato_tools`).
#[test]
fn agentfield_module_exposes_no_tool_implementation() {
    let mod_rs = include_str!("../src/agentfield/mod.rs");
    for expected in [
        "pub mod config;",
        "pub mod client;",
        "pub mod types;",
        "pub mod probe;",
    ] {
        assert!(
            mod_rs.contains(expected),
            "agentfield mod.rs is missing {expected}"
        );
    }
    let sources: &[(&str, &str)] = &[
        ("mod.rs", mod_rs),
        ("config.rs", include_str!("../src/agentfield/config.rs")),
        ("client.rs", include_str!("../src/agentfield/client.rs")),
        ("types.rs", include_str!("../src/agentfield/types.rs")),
        ("probe.rs", include_str!("../src/agentfield/probe.rs")),
    ];
    for (name, source) in sources {
        assert!(
            !source.contains("lato_core") && !source.contains("lato_tools"),
            "agentfield/{name} must not gain tool-protocol dependencies in 7C1"
        );
    }
}
