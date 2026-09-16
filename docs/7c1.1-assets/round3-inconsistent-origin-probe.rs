use async_trait::async_trait;
use lato_agent::agentfield::{client::ReqwestTransport, ControlPlaneOrigin};
use lato_mcp::McpDnsResolver;
use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

struct CountingResolver {
    calls: Arc<AtomicUsize>,
    addresses: Vec<IpAddr>,
}

#[async_trait]
impl McpDnsResolver for CountingResolver {
    async fn resolve(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .addresses
            .iter()
            .copied()
            .map(|ip| SocketAddr::new(ip, port))
            .collect())
    }
}

fn origin(url: &str, dev_mode: bool) -> ControlPlaneOrigin {
    ControlPlaneOrigin {
        base: url.parse().unwrap(),
        loopback_dev_mode: dev_mode,
    }
}

#[tokio::test]
async fn literal_loopback_is_rejected_before_dns() {
    for url in ["https://localhost", "https://127.0.0.2", "https://[::1]"] {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = CountingResolver {
            calls: calls.clone(),
            addresses: vec!["93.184.216.34".parse().unwrap()],
        };
        assert!(
            ReqwestTransport::connect_with_resolver(&origin(url, false), &resolver)
                .await
                .is_err(),
            "{url} was accepted"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{url} performed DNS");
    }
}

#[tokio::test]
async fn https_loopback_cannot_be_enabled_by_dev_flag() {
    let resolver = CountingResolver {
        calls: Arc::new(AtomicUsize::new(0)),
        addresses: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
    };
    assert!(
        ReqwestTransport::connect_with_resolver(
            &origin("https://127.0.0.1", true),
            &resolver,
        )
        .await
        .is_err(),
        "HTTPS loopback was accepted when the public dev flag was forged"
    );
}
