//! AgentField v1.2 boundary guards (spec §0.2 item 7, §0.3, §0.5, §8.1).
//!
//! Phase 7C2 amended: the main-session `agentfield` model tool is now
//! registered — in `host.rs` ONLY, behind the configuration gate. No other
//! tool wiring surface (subagent runner, skills, journal, workflow tool,
//! actor) may reference agentfield, and the adapter module must not ship
//! its own production outbound network path beyond `agentfield/transport.rs`.

/// 7C1/7C2 wiring boundary: `host.rs` is the ONLY main-session registration
/// surface; every other tool registration / runtime wiring surface stays
/// free of agentfield references (AC-01, registration test 3).
#[test]
fn agentfield_is_absent_from_every_non_host_wiring_surface() {
    let surfaces: &[(&str, &str)] = &[
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
            "{path} must not reference agentfield; \
             main-session registration belongs to host.rs only"
        );
    }
}

/// 7C2: the main-session registration must exist in host.rs, must be gated
/// on the validated multi-source config assembly (Round-3: registration and
/// the post-approval recheck share `assemble_catalog_config`), and must
/// install the manager on the session so close marks it unavailable.
#[test]
fn host_registers_agentfield_only_behind_the_config_gate() {
    let host = include_str!("../src/host.rs").to_ascii_lowercase();
    for expected in [
        "catalog_sources(",
        "assemble_catalog_config",
        "sessionagentfieldhandle",
        "agentfieldtool::new",
        "attach_agentfield_manager",
    ] {
        assert!(
            host.contains(expected),
            "host.rs is missing the 7C2 registration marker `{expected}`"
        );
    }
    // The adapter module stays free of tool-protocol dependencies in the
    // offline files (config/client/types/probe); catalog/manager/tool are
    // the sanctioned 7C2 additions.
    for (name, source) in [
        ("config.rs", include_str!("../src/agentfield/config.rs")),
        ("client.rs", include_str!("../src/agentfield/client.rs")),
        ("types.rs", include_str!("../src/agentfield/types.rs")),
        ("probe.rs", include_str!("../src/agentfield/probe.rs")),
        ("catalog.rs", include_str!("../src/agentfield/catalog.rs")),
    ] {
        assert!(
            !source.contains("lato_core") && !source.contains("lato_tools"),
            "agentfield/{name} must not gain tool-protocol dependencies"
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

/// 7C1.1 AC-08: the transport exposes exactly ONE public constructor (the
/// policy-enforcing factory) and no insecure/proxy/redirect customization
/// surface anywhere.
#[test]
fn production_transport_has_exactly_one_public_constructor_and_no_insecure_path() {
    let source = include_str!("../src/agentfield/transport.rs");
    let count = source.matches("pub async fn connect").count();
    assert_eq!(
        count, 1,
        "transport must expose exactly one public constructor"
    );
    assert!(
        !source.contains("pub fn new"),
        "no raw-client constructor may exist on the transport"
    );
    for forbidden in [
        "danger_",
        "accept_invalid",
        "Proxy::",
        "Policy::custom",
        "with_pinned_addrs",
    ] {
        assert!(
            !source.contains(forbidden),
            "transport must not contain `{forbidden}`"
        );
    }
}

/// 7C1.1 AC-08: the module's public surface routes product construction
/// through the policy factory, which resolves the credential BEFORE the
/// transport exists (unresolvable credential ⇒ zero network). The 7C2
/// token-based variant keeps the same ordering at the registration gate.
#[test]
fn product_construction_reaches_the_transport_only_through_the_policy_factory() {
    let mod_rs = include_str!("../src/agentfield/mod.rs");
    assert!(
        mod_rs.contains("pub async fn production_agentfield_client"),
        "mod.rs must expose the unique policy-enforcing factory"
    );
    let resolve_idx = mod_rs
        .find("resolve_agentfield_credential")
        .expect("factory must resolve the credential");
    let connect_idx = mod_rs
        .find("ReqwestTransport::connect")
        .expect("factory must build the pinned transport");
    assert!(
        resolve_idx < connect_idx,
        "credential resolution must precede transport construction"
    );
    // The public re-exports must not leak resolver or pinning internals.
    for forbidden in ["AgentFieldDnsResolver", "PinnedAddresses", "Limits"] {
        assert!(
            !mod_rs.contains(&format!("pub use transport::{forbidden}")),
            "mod.rs must not re-export internal `{forbidden}`"
        );
    }
}

/// 7C2: the module entry point wires the sanctioned sub-module set; the
/// offline Stage-1 files stay free of tool-protocol dependencies, while
/// `tool.rs` is the single Tool implementation of the adapter.
#[test]
fn agentfield_module_keeps_the_frozen_submodule_layout() {
    let mod_rs = include_str!("../src/agentfield/mod.rs");
    for expected in [
        "pub mod catalog;",
        "pub mod client;",
        "pub mod config;",
        "pub mod manager;",
        "pub mod probe;",
        "pub mod tool;",
        "pub mod transport;",
        "pub mod types;",
    ] {
        assert!(
            mod_rs.contains(expected),
            "agentfield mod.rs is missing {expected}"
        );
    }
    for forbidden in ["lato_core", "lato_tools"] {
        assert!(
            !mod_rs.contains(forbidden),
            "agentfield mod.rs must not reference {forbidden}"
        );
    }
}
