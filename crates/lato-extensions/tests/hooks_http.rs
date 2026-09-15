use std::{
    collections::BTreeMap,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use async_trait::async_trait;
use lato_extensions::hooks::{
    HandlerType, HookDnsResolver, HookEventEnvelope, HookEventName, HookRunContext, HookRunError,
    HookSpec, run_http_hook, validate_hook_url,
};
use tokio_util::sync::CancellationToken;
use url::Url;

struct Resolver(Vec<SocketAddr>);

#[async_trait]
impl HookDnsResolver for Resolver {
    async fn resolve(&self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(self.0.clone())
    }
}

fn resolver(ip: IpAddr) -> Resolver {
    Resolver(vec![SocketAddr::new(ip, 443)])
}

#[tokio::test]
async fn rejects_non_https_credentials_and_blocked_networks() {
    let public = resolver(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)));
    assert!(matches!(
        validate_hook_url(&Url::parse("http://example.com").unwrap(), &public).await,
        Err(HookRunError::UnsafeUrl)
    ));
    assert!(matches!(
        validate_hook_url(&Url::parse("https://u:p@example.com").unwrap(), &public).await,
        Err(HookRunError::UnsafeUrl)
    ));
    for ip in [
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1)),
        IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped()),
        IpAddr::V6("fd00::1".parse().unwrap()),
    ] {
        assert!(
            matches!(
                validate_hook_url(&Url::parse("https://example.com").unwrap(), &resolver(ip)).await,
                Err(HookRunError::UnsafeUrl)
            ),
            "{ip}"
        );
    }
}

#[tokio::test]
async fn allows_public_and_loopback_but_rejects_any_mixed_blocked_answer() {
    let url = Url::parse("https://example.com/hook").unwrap();
    assert_eq!(
        validate_hook_url(&url, &resolver(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))))
            .await
            .unwrap(),
        vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 443)]
    );
    assert!(
        validate_hook_url(&url, &resolver(IpAddr::V4(Ipv4Addr::LOCALHOST)))
            .await
            .is_ok()
    );
    assert!(
        validate_hook_url(&url, &resolver(IpAddr::V6(Ipv6Addr::LOCALHOST)))
            .await
            .is_ok()
    );
    let mixed = Resolver(vec![
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 443),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 443),
    ]);
    assert!(validate_hook_url(&url, &mixed).await.is_err());
}

#[test]
fn api_types_remain_constructible_without_leaking_urls() {
    let _ = BTreeMap::<String, String>::new();
    let _ = Path::new(".");
    assert_eq!(
        HookRunError::UnsafeUrl.to_string(),
        "hook URL is not allowed"
    );
}

struct CountingResolver {
    addresses: Vec<SocketAddr>,
    calls: AtomicUsize,
}

#[async_trait]
impl HookDnsResolver for CountingResolver {
    async fn resolve(&self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Ok(self.addresses.clone())
    }
}

fn http_spec(url: String) -> HookSpec {
    HookSpec {
        id: "pin".into(),
        plugin_name: "p".into(),
        event: HookEventName::PreToolUse,
        handler_type: HandlerType::Http,
        matcher: None,
        command: None,
        url: Some(url),
        timeout_ms: 2_000,
        source_dir: Path::new(".").to_path_buf(),
        extra_env: BTreeMap::new(),
    }
}

fn http_envelope() -> HookEventEnvelope {
    HookEventEnvelope::new(
        HookEventName::PreToolUse,
        1,
        "s",
        Some("t".into()),
        serde_json::json!({"toolName":"read_file"}),
    )
    .unwrap()
}

fn http_ctx() -> HookRunContext<'static> {
    HookRunContext {
        session_id: "s",
        workspace_root: Path::new("."),
        cancellation: CancellationToken::new(),
    }
}

#[tokio::test]
async fn http_hook_pins_validated_addresses_and_does_not_re_resolve() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = tokio::spawn(async move {
        let (stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("HTTPS client must connect to the validated address, not re-resolve")
            .expect("listener must accept the pinned connection");
        drop(stream);
    });
    let resolver = CountingResolver {
        addresses: vec![addr],
        calls: AtomicUsize::new(0),
    };
    let spec = http_spec(format!(
        "https://pin-test.example.invalid:{}/hook",
        addr.port()
    ));
    let result = run_http_hook(&spec, &http_envelope(), &http_ctx(), &resolver).await;
    assert!(
        matches!(result, Err(HookRunError::Http)),
        "pinned loopback TLS handshake must fail closed: {result:?}"
    );
    assert_eq!(resolver.calls.load(Ordering::Acquire), 1);
    accepted.await.unwrap();
}

#[tokio::test]
async fn http_hook_does_not_connect_when_resolution_is_blocked() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_millis(200), listener.accept()).await
    });
    let resolver = CountingResolver {
        addresses: vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            addr.port(),
        )],
        calls: AtomicUsize::new(0),
    };
    let spec = http_spec(format!(
        "https://pin-test.example.invalid:{}/hook",
        addr.port()
    ));
    let result = run_http_hook(&spec, &http_envelope(), &http_ctx(), &resolver).await;
    assert!(matches!(result, Err(HookRunError::UnsafeUrl)));
    assert_eq!(resolver.calls.load(Ordering::Acquire), 1);
    assert!(
        accepted.await.unwrap().is_err(),
        "blocked resolution must not produce a TCP connection"
    );
}
