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

/// v1.2.1 boundary, narrowed by Phase 7C1.1: the offline AgentField files
/// must still contain no direct network I/O of their own — the ONLY outbound
/// path is `agentfield/transport.rs` behind its policy-enforcing factory.
/// These source-level guards fail the build if that boundary regresses.
#[test]
fn agentfield_offline_files_have_no_direct_network_path() {
    let sources: &[(&str, &str)] = &[
        ("config.rs", include_str!("../src/agentfield/config.rs")),
        ("client.rs", include_str!("../src/agentfield/client.rs")),
        ("types.rs", include_str!("../src/agentfield/types.rs")),
        ("probe.rs", include_str!("../src/agentfield/probe.rs")),
    ];
    let forbidden = [
        "reqwest",
        "ReqwestTransport",
        "TcpStream",
        "lookup_host",
        "tokio::net",
        "lato_mcp",
        "McpDnsResolver",
    ];
    for (name, source) in sources {
        for marker in forbidden {
            assert!(
                !source.contains(marker),
                "agentfield/{name} must not reference `{marker}`: \
                 outbound network belongs to agentfield/transport.rs"
            );
        }
    }
}

/// Phase 7C1.1: the production transport must keep its frozen policy —
/// rustls verification, no redirects, no proxy, bounded headers/timeouts —
/// and its arbitrary-resolver seam must stay inside `#[cfg(test)]`.
#[test]
fn production_transport_keeps_frozen_policy_and_test_only_seam() {
    let source = include_str!("../src/agentfield/transport.rs");
    for expected in [
        ".use_rustls_tls()",
        ".redirect(Policy::none())",
        ".no_proxy()",
        ".connect_timeout(",
        ".read_timeout(",
        "MAX_RESOLVED_ADDRESSES",
        "classify_resolution",
        "PinnedAddresses",
    ] {
        assert!(
            source.contains(expected),
            "agentfield/transport.rs is missing frozen policy `{expected}`"
        );
    }
    for forbidden in [
        "danger_accept_invalid",
        "accept_invalid_certs",
        "Policy::custom",
        "Proxy::",
        "with_pinned_addrs",
    ] {
        assert!(
            !source.contains(forbidden),
            "agentfield/transport.rs must not contain `{forbidden}`"
        );
    }
    // The arbitrary-resolver seam must be defined after (inside) the test
    // section so a default build cannot reach it.
    let test_gate = source
        .find("#[cfg(test)]")
        .expect("transport.rs must carry a cfg(test) test section");
    let seam = source
        .find("connect_with_resolver")
        .expect("transport.rs must define the cfg(test) resolver seam");
    assert!(
        seam > test_gate,
        "connect_with_resolver must be defined inside the #[cfg(test)] section"
    );
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
