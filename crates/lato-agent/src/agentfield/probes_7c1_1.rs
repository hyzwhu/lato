// WIN-28 Round 2/3 acceptance probes, restored as permanent regressions
// (Phase 7C1.1, AC-10). Source originals: `docs/7c1.1-assets/
// round2-https-loopback-probe.rs` and `round3-inconsistent-origin-probe.rs`.
// The probes run inside `#[cfg(test)]` against the arbitrary-resolver seam —
// exactly the surface the strict acceptance officer probed; a default build
// cannot reach any of it.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use super::transport::ReqwestTransport;
use super::transport::tests::{CountingResolver, FixedResolver};
use crate::agentfield::config::ControlPlaneOrigin;

/// Round 2 probe, part 1: production HTTPS must reject a hostname whose DNS
/// answer points at `127.0.0.1`.
#[tokio::test]
async fn round2_production_https_hostname_resolving_loopback_must_fail_closed() {
    let resolver = Arc::new(FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]));
    let result = ReqwestTransport::connect_with_resolver(
        &https_origin("https://agents.example.internal"),
        resolver,
    )
    .await;
    assert!(
        result.is_err(),
        "production HTTPS resolved to loopback was accepted"
    );
}

/// Round 2 probe, part 2: production HTTPS must reject a loopback literal.
#[tokio::test]
async fn round2_production_https_literal_loopback_must_fail_closed() {
    let resolver = Arc::new(FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]));
    let result =
        ReqwestTransport::connect_with_resolver(&https_origin("https://127.0.0.1"), resolver).await;
    assert!(
        result.is_err(),
        "production HTTPS loopback literal was accepted"
    );
}

/// Round 3 probe, part 1: loopback literals are rejected BEFORE any DNS
/// query (zero resolver calls, zero DNS leakage).
#[tokio::test]
async fn round3_literal_loopback_is_rejected_before_dns() {
    for url in ["https://localhost", "https://127.0.0.2", "https://[::1]"] {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = CountingResolver {
            calls: calls.clone(),
            answers: Mutex::new(vec![vec![IpAddr::from([93, 184, 216, 34])]]),
        };
        let resolver = Arc::new(resolver);
        assert!(
            ReqwestTransport::connect_with_resolver(&https_origin(url), resolver)
                .await
                .is_err(),
            "{url} was accepted"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{url} performed DNS");
    }
}

/// Round 3 probe, part 2: a public development flag cannot unlock HTTPS to a
/// loopback target (only plain-HTTP loopback in explicit development mode).
#[tokio::test]
async fn round3_https_loopback_cannot_be_enabled_by_dev_flag() {
    let resolver = Arc::new(FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]));
    let forged = ControlPlaneOrigin {
        base: "https://127.0.0.1".parse().unwrap(),
        allow_loopback_http: true,
    };
    assert!(
        ReqwestTransport::connect_with_resolver(&forged, resolver)
            .await
            .is_err(),
        "HTTPS loopback was accepted when the public dev flag was forged"
    );
}

fn https_origin(url: &str) -> ControlPlaneOrigin {
    ControlPlaneOrigin {
        base: url.parse().unwrap(),
        allow_loopback_http: false,
    }
}
