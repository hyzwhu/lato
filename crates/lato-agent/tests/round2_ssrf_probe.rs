// Round 2 SSRF probe (provided by the strict acceptance officer, retained as
// a permanent regression test): production-mode HTTPS must reject loopback
// destinations — both a hostname resolving to `127.0.0.1` and a loopback
// literal in the URL.

use async_trait::async_trait;
use lato_agent::agentfield::{ControlPlaneOrigin, client::ReqwestTransport};
use lato_mcp::McpDnsResolver;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

struct FixedResolver(Vec<IpAddr>);

#[async_trait]
impl McpDnsResolver for FixedResolver {
    async fn resolve(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(self
            .0
            .iter()
            .copied()
            .map(|ip| SocketAddr::new(ip, port))
            .collect())
    }
}

fn origin(url: &str) -> ControlPlaneOrigin {
    ControlPlaneOrigin {
        base: url.parse().unwrap(),
        loopback_dev_mode: false,
    }
}

#[tokio::test]
async fn production_https_hostname_resolving_loopback_must_fail_closed() {
    let resolver = FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    let result = ReqwestTransport::connect_with_resolver(
        &origin("https://agents.example.internal"),
        &resolver,
    )
    .await;
    assert!(
        result.is_err(),
        "production HTTPS resolved to loopback was accepted"
    );
}

#[tokio::test]
async fn production_https_literal_loopback_must_fail_closed() {
    let resolver = FixedResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    let result =
        ReqwestTransport::connect_with_resolver(&origin("https://127.0.0.1"), &resolver).await;
    assert!(
        result.is_err(),
        "production HTTPS loopback literal was accepted"
    );
}
