use std::{
    collections::BTreeMap,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
};

use async_trait::async_trait;
use lato_extensions::hooks::{HookDnsResolver, HookRunError, validate_hook_url};
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
    assert!(
        validate_hook_url(&url, &resolver(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))))
            .await
            .is_ok()
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
