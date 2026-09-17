// Phase 7C1.1: production HTTPS transport with a frozen network policy
// (spec §0.3/§0.4). This module is the ONLY place in the workspace where a
// production AgentField HTTP client is constructed, and the constructor is a
// single policy-enforcing factory: every request is origin-checked,
// re-resolved, classified against the address policy, and pinned to the
// validated address set before any socket can be dialed. There is no public
// constructor that accepts a raw reqwest Client, an arbitrary resolver, a
// pinned address set, or disabled verification.
//
// Policy invariants (frozen for Phase 7C1.1):
// - TLS is rustls with default verification; insecure overrides do not exist.
// - Redirects are never followed (3xx answers are stable rejections).
// - Proxies are disabled outright (`no_proxy`): a proxy cannot re-route a
//   validated origin to an unvalidated address.
// - connect / idle / total timeouts and response header/body caps bound
//   every request; the body cap applies to the DECOMPRESSED byte stream.
// - DNS validation and connection share one frozen validated address set:
//   the reqwest client can only dial addresses from the set that was just
//   classified, so "validate A, connect B" is impossible.
// - Every `send` re-resolves and re-classifies; connection reuse can never
//   cross origins (one client, one origin, URL-checked per request).
//
// The module is delivered before its product wiring (the public factory is
// enabled together with the DNS-pinning policy in the next commit); until
// then the compiler cannot see the usage edge, hence the scoped dead_code
// allowance below. It is removed when the factory becomes public.

// Phase-7C1.1 delivery ordering: transport is built and tested before the
// public factory wiring is switched on; nothing in the product can reach it.
#![allow(dead_code)]

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::redirect::Policy;
use reqwest::{Client, Method};

use crate::agentfield::client::{HttpTransport, OutboundRequest, RawResponse, TransportError};
use crate::agentfield::config::ControlPlaneOrigin;

/// A resolver returning more than this many addresses fails closed
/// (runaway or hostile DNS answers are rejected wholesale).
pub(crate) const MAX_RESOLVED_ADDRESSES: usize = 8;

/// Application-level response header cap. The HTTP/1 parser below this
/// (hyper) already enforces its own hard limit of 100 headers; Lato refuses
/// anything beyond the stricter bound here.
pub(crate) const MAX_RESPONSE_HEADERS: usize = 64;

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Idle gap between response bytes (slowloris-style trickles are cut here).
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Whole-request budget including DNS, connect, TLS, and the body stream.
pub(crate) const TOTAL_TIMEOUT: Duration = Duration::from_secs(60);

/// Test/production DNS seam. Production always uses [`SystemAgentFieldDnsResolver`];
/// arbitrary resolvers are only reachable from `#[cfg(test)]`.
#[async_trait]
pub(crate) trait AgentFieldDnsResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

pub(crate) struct SystemAgentFieldDnsResolver;

#[async_trait]
impl AgentFieldDnsResolver for SystemAgentFieldDnsResolver {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }
}

/// Why one resolved address is not dialable. The description is stable and
/// safe for diagnostics (no addresses are echoed).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AddressRejection(pub(crate) &'static str);

fn classify_v4(ip: std::net::Ipv4Addr, http_dev_loopback: bool) -> Result<(), AddressRejection> {
    if ip.is_loopback() {
        return if http_dev_loopback {
            Ok(())
        } else {
            Err(AddressRejection("loopback address"))
        };
    }
    let octets = ip.octets();
    let blocked = ip.is_unspecified()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || octets[0] == 0
        // CGNAT 100.64.0.0/10
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        // 192.0.0.0/24 (IETF protocol assignments)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        // Benchmarking 198.18.0.0/15
        || (octets[0] == 198 && matches!(octets[1], 18 | 19))
        // Documentation ranges 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24
        || octets[0..3] == [192, 0, 2]
        || octets[0..3] == [198, 51, 100]
        || octets[0..3] == [203, 0, 113];
    if blocked {
        Err(AddressRejection("non-public address"))
    } else {
        Ok(())
    }
}

fn classify_v6(ip: std::net::Ipv6Addr, http_dev_loopback: bool) -> Result<(), AddressRejection> {
    if ip.is_loopback() {
        return if http_dev_loopback {
            Ok(())
        } else {
            Err(AddressRejection("loopback address"))
        };
    }
    // IPv4-mapped IPv6 must obey the IPv4 policy (mapped private ranges are
    // never a way around the v4 classification).
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return classify_v4(mapped, http_dev_loopback);
    }
    let segments = ip.segments();
    let blocked = ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        // Documentation 2001:db8::/32
        || (segments[0] == 0x2001 && segments[1] == 0x0db8);
    if blocked {
        Err(AddressRejection("non-public address"))
    } else {
        Ok(())
    }
}

/// Classify one address against the frozen policy. Loopback is dialable only
/// for plain HTTP in explicit development mode; production HTTPS rejects it
/// even when a development flag is set (Round 2/3 acceptance probes).
pub(crate) fn classify_address(
    ip: IpAddr,
    scheme: &str,
    allow_loopback_http: bool,
) -> Result<(), AddressRejection> {
    let http_dev_loopback = scheme == "http" && allow_loopback_http;
    match ip {
        IpAddr::V4(ip) => classify_v4(ip, http_dev_loopback),
        IpAddr::V6(ip) => classify_v6(ip, http_dev_loopback),
    }
}

/// What the origin host turned out to be after syntax checks: an IP
/// (or `localhost`) literal classified without DNS, or a hostname that
/// must be resolved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostClassification {
    Literal(IpAddr),
    Name,
}

/// Syntax-level host checks that run before any DNS query: zone IDs,
/// percent-encoding, Unicode confusion labels and trailing-dot bypasses are
/// rejected outright; IP literals and `localhost` are classified directly
/// with zero DNS queries (Round 3 probe).
pub(crate) fn classify_host_literal(
    host: &str,
    scheme: &str,
    allow_loopback_http: bool,
) -> Result<HostClassification, AddressRejection> {
    if host.is_empty() {
        return Err(AddressRejection("empty host"));
    }
    if host.contains('%') {
        return Err(AddressRejection("zone id or percent-encoding in host"));
    }
    if host.ends_with('.') {
        return Err(AddressRejection("trailing-dot host"));
    }
    if !host.is_ascii() || host.contains("xn--") {
        return Err(AddressRejection("non-canonical (IDNA/Unicode) host"));
    }
    // `localhost` is a loopback literal regardless of what DNS claims.
    if host == "localhost" {
        classify_address(IpAddr::from([127, 0, 0, 1]), scheme, allow_loopback_http)?;
        return Ok(HostClassification::Literal(IpAddr::from([127, 0, 0, 1])));
    }
    let literal = if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        Some(IpAddr::V6(
            inner
                .parse()
                .map_err(|_| AddressRejection("invalid IPv6 literal"))?,
        ))
    } else {
        host.parse::<IpAddr>().ok()
    };
    if let Some(ip) = literal {
        classify_address(ip, scheme, allow_loopback_http)?;
        return Ok(HostClassification::Literal(ip));
    }
    Ok(HostClassification::Name)
}

/// Classify a full resolution answer. Mixed answers fail closed: a single
/// forbidden address rejects the whole set and no connection is attempted.
pub(crate) fn classify_resolution(
    addresses: &[SocketAddr],
    scheme: &str,
    allow_loopback_http: bool,
) -> Result<(), AddressRejection> {
    if addresses.is_empty() {
        return Err(AddressRejection("empty DNS answer"));
    }
    if addresses.len() > MAX_RESOLVED_ADDRESSES {
        return Err(AddressRejection("oversized DNS answer"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for address in addresses {
        if !seen.insert(address.ip()) {
            return Err(AddressRejection("duplicate DNS answer"));
        }
        classify_address(address.ip(), scheme, allow_loopback_http)?;
    }
    Ok(())
}

/// The frozen validated address set served to the reqwest client. DNS
/// validation and connection share this exact set, so the client can never
/// dial an address that was not just classified.
struct PinnedAddresses {
    host: String,
    set: Arc<Mutex<Vec<SocketAddr>>>,
}

impl PinnedAddresses {
    fn freeze(&self, addresses: Vec<SocketAddr>) {
        *self.set.lock().unwrap_or_else(|error| error.into_inner()) = addresses;
    }
}

impl Resolve for PinnedAddresses {
    fn resolve(&self, name: Name) -> Resolving {
        let name = name.as_str().to_string();
        let host = self.host.clone();
        let set = self.set.clone();
        Box::pin(async move {
            if !name.eq_ignore_ascii_case(&host) {
                let error: Box<dyn std::error::Error + Send + Sync> =
                    io::Error::other("pinned address set is bound to the configured origin").into();
                return Err(error);
            }
            let addresses = set
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let addrs: Addrs = Box::new(addresses.into_iter());
            Ok(addrs)
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) connect_timeout: Duration,
    pub(crate) idle_timeout: Duration,
    pub(crate) total_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            connect_timeout: CONNECT_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
            total_timeout: TOTAL_TIMEOUT,
        }
    }
}

/// Production AgentField transport. One instance is bound to one validated
/// origin; it owns its reqwest client (rustls, no redirects, no proxy) and
/// the frozen address set the client may dial.
pub struct ReqwestTransport {
    origin: ControlPlaneOrigin,
    /// `Some` when the origin host is an IP/`localhost` literal: the policy
    /// then re-classifies the literal on every send and never queries DNS.
    literal: Option<IpAddr>,
    client: Client,
    pinned: Arc<PinnedAddresses>,
    resolver: Arc<dyn AgentFieldDnsResolver>,
    limits: Limits,
}

impl ReqwestTransport {
    /// The unique production constructor. Resolves the configured host once,
    /// classifies the full answer, and pins the client to the validated set.
    /// Every later `send` re-runs the same policy before dialing.
    pub(crate) async fn connect(origin: &ControlPlaneOrigin) -> Result<Self, TransportError> {
        Self::connect_with_limits(origin, Limits::default()).await
    }

    pub(crate) async fn connect_with_limits(
        origin: &ControlPlaneOrigin,
        limits: Limits,
    ) -> Result<Self, TransportError> {
        Self::build(origin, Arc::new(SystemAgentFieldDnsResolver), limits).await
    }

    /// Arbitrary-resolver construction exists only for in-crate regression
    /// tests (Round 2/3 probes); the default build cannot reach it.
    #[cfg(test)]
    pub(crate) async fn connect_with_resolver(
        origin: &ControlPlaneOrigin,
        resolver: Arc<dyn AgentFieldDnsResolver>,
    ) -> Result<Self, TransportError> {
        Self::build(origin, resolver, Limits::default()).await
    }

    async fn build(
        origin: &ControlPlaneOrigin,
        resolver: Arc<dyn AgentFieldDnsResolver>,
        limits: Limits,
    ) -> Result<Self, TransportError> {
        let host = origin_host(origin)?;
        let scheme = origin.base.scheme();
        let port = origin
            .base
            .port_or_known_default()
            .ok_or(TransportError::PolicyRejected("origin port unknown".into()))?;
        // Literal hosts are classified without any DNS query; a hostname is
        // resolved once and the full answer classified before a client exists.
        let literal = match classify_host_literal(&host, scheme, origin.allow_loopback_http)
            .map_err(reject)?
        {
            HostClassification::Literal(ip) => {
                classify_resolution(
                    &[SocketAddr::new(ip, port)],
                    scheme,
                    origin.allow_loopback_http,
                )
                .map_err(reject)?;
                Some(ip)
            }
            HostClassification::Name => None,
        };
        let addresses = match literal {
            Some(ip) => vec![SocketAddr::new(ip, port)],
            None => resolver
                .resolve(&host, port)
                .await
                .map_err(|_| TransportError::Connection("DNS resolution failed".into()))?,
        };
        classify_resolution(&addresses, scheme, origin.allow_loopback_http).map_err(reject)?;
        let pinned = Arc::new(PinnedAddresses {
            host,
            set: Arc::new(Mutex::new(addresses)),
        });
        let client = Client::builder()
            // Frozen TLS policy: rustls with default verification. No
            // insecure-certificate path exists anywhere in this module.
            .use_rustls_tls()
            .redirect(Policy::none())
            .no_proxy()
            .connect_timeout(limits.connect_timeout)
            .read_timeout(limits.idle_timeout)
            .dns_resolver(pinned.clone())
            .build()
            .map_err(|_| TransportError::Connection("HTTP client construction failed".into()))?;
        Ok(Self {
            origin: origin.clone(),
            literal,
            client,
            pinned,
            resolver,
            limits,
        })
    }

    /// Re-resolve and re-classify before dialing, then freeze the fresh set.
    /// A single forbidden address aborts the request with zero connections.
    /// Literal origins re-classify the literal instead of querying DNS.
    async fn revalidate(&self) -> Result<(), TransportError> {
        let scheme = self.origin.base.scheme();
        let port = self
            .origin
            .base
            .port_or_known_default()
            .ok_or(TransportError::PolicyRejected("origin port unknown".into()))?;
        let addresses = match self.literal {
            Some(ip) => {
                classify_address(ip, scheme, self.origin.allow_loopback_http).map_err(reject)?;
                vec![SocketAddr::new(ip, port)]
            }
            None => {
                let host = origin_host(&self.origin)?;
                let resolved = self
                    .resolver
                    .resolve(&host, port)
                    .await
                    .map_err(|_| TransportError::Connection("DNS resolution failed".into()))?;
                classify_resolution(&resolved, scheme, self.origin.allow_loopback_http)
                    .map_err(reject)?;
                resolved
            }
        };
        self.pinned.freeze(addresses);
        Ok(())
    }

    /// Origin equality for every outgoing request: scheme, lowercase host,
    /// and effective port must match the configured origin exactly; userinfo,
    /// query strings, fragments, zone IDs and non-canonical hosts are
    /// rejected before any I/O.
    fn validate_request_url(&self, url: &str) -> Result<reqwest::Url, TransportError> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|_| TransportError::PolicyRejected("request URL is not a URL".into()))?;
        if parsed.username() != "" || parsed.password().is_some() {
            return Err(TransportError::PolicyRejected(
                "request URL carries userinfo".into(),
            ));
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(TransportError::PolicyRejected(
                "request URL carries query or fragment".into(),
            ));
        }
        let host = parsed.host_str().ok_or(TransportError::PolicyRejected(
            "request URL has no host".into(),
        ))?;
        let origin_host = origin_host(&self.origin)?;
        if !host.eq_ignore_ascii_case(&origin_host) {
            return Err(TransportError::PolicyRejected(
                "request host does not match the configured origin".into(),
            ));
        }
        if parsed.scheme() != self.origin.base.scheme() {
            return Err(TransportError::PolicyRejected(
                "request scheme does not match the configured origin".into(),
            ));
        }
        if parsed.port_or_known_default() != self.origin.base.port_or_known_default() {
            return Err(TransportError::PolicyRejected(
                "request port does not match the configured origin".into(),
            ));
        }
        Ok(parsed)
    }

    async fn send_inner(&self, request: OutboundRequest) -> Result<RawResponse, TransportError> {
        let url = self.validate_request_url(&request.url)?;
        self.revalidate().await?;
        let method = Method::from_bytes(request.method.as_bytes())
            .map_err(|_| TransportError::PolicyRejected("invalid HTTP method".into()))?;
        let mut builder = self.client.request(method, url);
        if let Some(body) = &request.json_body {
            let bytes = serde_json::to_vec(body)
                .map_err(|_| TransportError::PolicyRejected("request body is not JSON".into()))?;
            builder = builder.header(CONTENT_TYPE, "application/json").body(bytes);
        }
        // Credential injection happens exactly here, at the final request
        // construction point; the token never touches logs, errors, or URLs.
        if let Some(bearer) = &request.bearer {
            builder = builder.bearer_auth(bearer.expose());
        }
        let response = builder.send().await.map_err(sanitize_send_error)?;
        let status = response.status();
        if status.is_redirection() {
            // Redirects are never followed: same-origin, cross-origin, and
            // HTTPS→HTTP downgrades all land here (reqwest answers with the
            // 3xx itself because the policy is `Policy::none`).
            return Err(TransportError::RedirectRejected(format!(
                "control plane answered redirect {}",
                status.as_u16()
            )));
        }
        let headers = response.headers();
        if headers.len() > MAX_RESPONSE_HEADERS {
            return Err(TransportError::Protocol(format!(
                "response carries more than {MAX_RESPONSE_HEADERS} headers"
            )));
        }
        let wire_length = headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok());
        if wire_length
            .is_some_and(|length| length > crate::agentfield::client::MAX_RESPONSE_BODY_BYTES)
        {
            return Err(TransportError::BodyTooLarge(
                crate::agentfield::client::MAX_RESPONSE_BODY_BYTES,
            ));
        }
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_string());
        // Stream the (already decompressed) body under the hard cap; the
        // counter bounds decompression bombs and unterminated chunked bodies.
        let mut response = response;
        let mut body: Vec<u8> = Vec::new();
        loop {
            let chunk = response.chunk().await.map_err(sanitize_stream_error)?;
            let Some(chunk) = chunk else { break };
            if body.len() + chunk.len() > crate::agentfield::client::MAX_RESPONSE_BODY_BYTES {
                return Err(TransportError::BodyTooLarge(
                    crate::agentfield::client::MAX_RESPONSE_BODY_BYTES,
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(RawResponse {
            status: status.as_u16(),
            content_type,
            body,
        })
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn send(&self, request: OutboundRequest) -> Result<RawResponse, TransportError> {
        let operation = self.send_inner(request);
        match tokio::time::timeout(self.limits.total_timeout, operation).await {
            Ok(result) => result,
            Err(_) => Err(TransportError::Connection(
                "request exceeded the total timeout budget".into(),
            )),
        }
    }
}

fn origin_host(origin: &ControlPlaneOrigin) -> Result<String, TransportError> {
    let host = origin
        .base
        .host_str()
        .ok_or(TransportError::PolicyRejected("origin has no host".into()))?;
    classify_host_literal(host, origin.base.scheme(), origin.allow_loopback_http)
        .map_err(reject)?;
    Ok(host.to_ascii_lowercase())
}

fn reject(rejection: AddressRejection) -> TransportError {
    TransportError::PolicyRejected(format!("address policy violation: {}", rejection.0))
}

/// reqwest error text may embed URLs; diagnostics must not leak internal
/// address details, so errors are reduced to a stable classification.
fn sanitize_send_error(error: reqwest::Error) -> TransportError {
    let kind = if error.is_timeout() {
        "request timed out"
    } else if error.is_connect() {
        "connection failed (DNS/TLS/connect)"
    } else if error.is_request() {
        "request could not be built"
    } else {
        "request failed before a response arrived"
    };
    TransportError::Connection(kind.into())
}

fn sanitize_stream_error(error: reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Connection("response stalled beyond the idle timeout".into())
    } else if error.is_decode() {
        TransportError::Protocol("response body could not be decoded".into())
    } else {
        TransportError::Connection("response stream failed".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentfield::config::ControlPlaneOrigin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FixedResolver(Vec<IpAddr>);

    #[async_trait]
    impl AgentFieldDnsResolver for FixedResolver {
        async fn resolve(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            Ok(self.0.iter().map(|ip| SocketAddr::new(*ip, port)).collect())
        }
    }

    struct CountingResolver {
        calls: AtomicUsize,
        answers: Mutex<Vec<Vec<IpAddr>>>,
    }

    impl CountingResolver {
        fn new(answers: Vec<Vec<IpAddr>>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                answers: Mutex::new(answers),
            }
        }
    }

    #[async_trait]
    impl AgentFieldDnsResolver for CountingResolver {
        async fn resolve(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let answer = self
                .answers
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pop()
                .unwrap_or_default();
            Ok(answer.iter().map(|ip| SocketAddr::new(*ip, port)).collect())
        }
    }

    fn https_origin(url: &str) -> ControlPlaneOrigin {
        ControlPlaneOrigin {
            base: url.parse().unwrap(),
            allow_loopback_http: false,
        }
    }

    fn dev_http_origin(url: &str) -> ControlPlaneOrigin {
        ControlPlaneOrigin {
            base: url.parse().unwrap(),
            allow_loopback_http: true,
        }
    }

    #[tokio::test]
    async fn literal_loopback_is_rejected_before_dns() {
        let resolver = Arc::new(CountingResolver::new(vec![vec![IpAddr::from([
            203, 0, 113, 10,
        ])]]));
        for url in ["https://localhost", "https://127.0.0.2", "https://[::1]"] {
            let result =
                ReqwestTransport::connect_with_resolver(&https_origin(url), resolver.clone()).await;
            assert!(result.is_err(), "{url} was accepted");
        }
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0, "DNS was queried");
    }

    #[test]
    fn production_https_rejects_every_private_range() {
        let cases = [
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "127.0.0.1",
            "0.0.0.0",
            "169.254.1.1",
            "100.64.0.1",
            "192.0.0.1",
            "198.18.0.1",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "255.255.255.255",
        ];
        for ip in cases {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(
                classify_address(ip, "https", false).is_err(),
                "{ip} was accepted in production"
            );
        }
    }

    #[test]
    fn production_https_rejects_non_public_v6() {
        let cases = [
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
            "::ffff:192.168.0.1",
        ];
        for ip in cases {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(
                classify_address(ip, "https", false).is_err(),
                "{ip} was accepted in production"
            );
        }
    }

    #[test]
    fn public_addresses_are_allowed_in_production() {
        for ip in ["93.184.216.34", "2606:2800:220:1:248:1893:25c8:1946"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(
                classify_address(ip, "https", false).is_ok(),
                "{ip} rejected"
            );
        }
    }

    #[test]
    fn host_syntax_rejects_zone_ids_trailing_dots_and_idna() {
        let cases = [
            "fe80::1%eth0",
            "example.com%2Fevil",
            "example.com.",
            "xn--e1afmkfd.example",
            "exämple.com",
        ];
        for host in cases {
            assert!(
                classify_host_literal(host, "https", false).is_err(),
                "{host} was accepted"
            );
        }
        // IP literals are classified directly (no DNS later for them).
        assert!(classify_host_literal("127.0.0.1", "https", false).is_err());
        assert!(classify_host_literal("agents.example.com", "https", false).is_ok());
    }

    #[tokio::test]
    async fn hostname_resolving_loopback_fails_closed_in_production() {
        let resolver = FixedResolver(vec![IpAddr::from([127, 0, 0, 1])]);
        let result = ReqwestTransport::connect_with_resolver(
            &https_origin("https://agents.example.internal"),
            Arc::new(resolver),
        )
        .await;
        assert!(
            result.is_err(),
            "production HTTPS resolved to loopback was accepted"
        );
    }

    #[tokio::test]
    async fn https_loopback_cannot_be_enabled_by_the_dev_flag() {
        let resolver = FixedResolver(vec![IpAddr::from([127, 0, 0, 1])]);
        let result = ReqwestTransport::connect_with_resolver(
            &ControlPlaneOrigin {
                base: "https://127.0.0.1".parse().unwrap(),
                allow_loopback_http: true,
            },
            Arc::new(resolver),
        )
        .await;
        assert!(
            result.is_err(),
            "HTTPS loopback was accepted with a forged dev flag"
        );
    }

    #[tokio::test]
    async fn empty_duplicate_and_oversized_answers_fail_closed() {
        let empty = FixedResolver(vec![]);
        assert!(
            ReqwestTransport::connect_with_resolver(
                &https_origin("https://agents.example.com"),
                Arc::new(empty)
            )
            .await
            .is_err()
        );
        let duplicate = FixedResolver(vec![
            IpAddr::from([93, 184, 216, 34]),
            IpAddr::from([93, 184, 216, 34]),
        ]);
        assert!(
            ReqwestTransport::connect_with_resolver(
                &https_origin("https://agents.example.com"),
                Arc::new(duplicate)
            )
            .await
            .is_err()
        );
        let oversized = FixedResolver(vec![
            IpAddr::from([93, 184, 216, 34]),
            IpAddr::from([93, 184, 216, 35]),
            IpAddr::from([93, 184, 216, 36]),
            IpAddr::from([93, 184, 216, 37]),
            IpAddr::from([93, 184, 216, 38]),
            IpAddr::from([93, 184, 216, 39]),
            IpAddr::from([93, 184, 216, 40]),
            IpAddr::from([93, 184, 216, 41]),
            IpAddr::from([93, 184, 216, 42]),
        ]);
        assert!(
            ReqwestTransport::connect_with_resolver(
                &https_origin("https://agents.example.com"),
                Arc::new(oversized)
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn mixed_answers_fail_closed_with_zero_resolver_side_effects() {
        // First answer is mixed public+private: the whole request fails closed.
        let resolver = Arc::new(CountingResolver::new(vec![vec![
            IpAddr::from([93, 184, 216, 34]),
            IpAddr::from([10, 0, 0, 5]),
        ]]));
        let result = ReqwestTransport::connect_with_resolver(
            &https_origin("https://agents.example.com"),
            resolver.clone(),
        )
        .await;
        assert!(result.is_err(), "mixed answer was accepted");
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }

    // --- HTTP mechanics against a local loopback server (dev-HTTP policy) ---

    /// Minimal canned-response server: one response per connection, then
    /// `connection: close`. Counts every accepted connection.
    struct CannedServer {
        addr: std::net::SocketAddr,
        connections: Arc<AtomicUsize>,
    }

    fn http_response(status_line: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "{status_line}\r\nconnection: close\r\ncontent-length: {}\r\n",
            body.len()
        )
        .into_bytes();
        for (name, value) in headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(body);
        out
    }

    async fn read_request_headers(sock: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 2048];
        let mut acc: Vec<u8> = Vec::new();
        loop {
            let read = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf))
                .await
                .expect("server read timed out")
                .expect("server read failed");
            if read == 0 {
                return;
            }
            acc.extend_from_slice(&buf[..read]);
            let header_end = acc
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|pos| pos + 4);
            if let Some(end) = header_end {
                let text = String::from_utf8_lossy(&acc[..end]).to_ascii_lowercase();
                let content_length = text
                    .split("\r\n")
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if acc.len() >= end + content_length {
                    return;
                }
            }
        }
    }

    async fn spawn_canned(responses: Vec<Vec<u8>>) -> CannedServer {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = connections.clone();
        tokio::spawn(async move {
            let mut queue: std::vec::IntoIter<Vec<u8>> = responses.into_iter();
            while let Ok((mut sock, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                read_request_headers(&mut sock).await;
                let Some(response) = queue.next() else { break };
                let _ = sock.write_all(&response).await;
                let _ = sock.shutdown().await;
            }
        });
        CannedServer { addr, connections }
    }

    fn dev_origin_for(addr: std::net::SocketAddr) -> ControlPlaneOrigin {
        ControlPlaneOrigin {
            base: format!("http://{}", addr).parse().unwrap(),
            allow_loopback_http: true,
        }
    }

    fn send_to(addr: std::net::SocketAddr, path: &str) -> OutboundRequest {
        OutboundRequest {
            method: "GET",
            url: format!("http://{addr}{path}"),
            bearer: None,
            json_body: None,
        }
    }

    #[tokio::test]
    async fn dev_http_loopback_request_round_trips() {
        let server = spawn_canned(vec![http_response(
            "HTTP/1.1 200 OK",
            &[("content-type", "application/json")],
            br#"{"ok":true}"#,
        )])
        .await;
        let transport = ReqwestTransport::connect_with_resolver(
            &dev_origin_for(server.addr),
            Arc::new(FixedResolver(vec![IpAddr::from([127, 0, 0, 1])])),
        )
        .await
        .unwrap();
        let response = transport
            .send(send_to(server.addr, "/api/v1/x"))
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, br#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn redirects_are_never_followed_and_produce_zero_followup_requests() {
        for status in ["301", "302", "307", "308"] {
            let server = spawn_canned(vec![http_response(
                &format!("HTTP/1.1 {status} Moved"),
                &[("location", "http://other-host.test/next")],
                b"",
            )])
            .await;
            let transport = ReqwestTransport::connect_with_resolver(
                &dev_origin_for(server.addr),
                Arc::new(FixedResolver(vec![IpAddr::from([127, 0, 0, 1])])),
            )
            .await
            .unwrap();
            let error = transport
                .send(send_to(server.addr, "/api/v1/x"))
                .await
                .unwrap_err();
            assert!(
                matches!(error, TransportError::RedirectRejected(_)),
                "{status}: {error:?}"
            );
            assert_eq!(
                server.connections.load(Ordering::SeqCst),
                1,
                "{status}: a follow-up request was issued"
            );
        }
    }

    #[tokio::test]
    async fn response_header_cap_is_enforced_at_two_layers() {
        // Above the Lato cap (64) but below the parser cap (100): the
        // transport rejects with a protocol violation.
        let many: Vec<(&'static str, &str)> = (0..90)
            .map(|index| {
                let name: &'static mut str = Box::leak(format!("x-h-{index}").into_boxed_str());
                let name: &'static str = &*name;
                (name, "v")
            })
            .collect();
        let server = spawn_canned(vec![http_response("HTTP/1.1 200 OK", &many, b"{}")]).await;
        let transport = ReqwestTransport::connect_with_resolver(
            &dev_origin_for(server.addr),
            Arc::new(FixedResolver(vec![IpAddr::from([127, 0, 0, 1])])),
        )
        .await
        .unwrap();
        let error = transport
            .send(send_to(server.addr, "/x"))
            .await
            .unwrap_err();
        assert!(matches!(error, TransportError::Protocol(_)), "{error:?}");

        // Above the parser cap (100): the HTTP/1 parser itself fails the
        // connection — the hard bound exists below the application layer too.
        let too_many: Vec<(&'static str, &str)> = (0..130)
            .map(|index| {
                let name: &'static mut str = Box::leak(format!("x-h-{index}").into_boxed_str());
                let name: &'static str = &*name;
                (name, "v")
            })
            .collect();
        let server = spawn_canned(vec![http_response("HTTP/1.1 200 OK", &too_many, b"{}")]).await;
        let transport = ReqwestTransport::connect_with_resolver(
            &dev_origin_for(server.addr),
            Arc::new(FixedResolver(vec![IpAddr::from([127, 0, 0, 1])])),
        )
        .await
        .unwrap();
        let error = transport
            .send(send_to(server.addr, "/x"))
            .await
            .unwrap_err();
        assert!(matches!(error, TransportError::Connection(_)), "{error:?}");
    }

    #[tokio::test]
    async fn wire_body_cap_rejects_oversized_content_length_before_reading() {
        // The server advertises more than the cap in content-length while
        // sending nothing: the cap must trip before any body read.
        let mut raw =
            b"HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: application/json\r\n".to_vec();
        raw.extend_from_slice(
            format!(
                "content-length: {}\r\n\r\n",
                crate::agentfield::client::MAX_RESPONSE_BODY_BYTES + 1
            )
            .as_bytes(),
        );
        let server = spawn_canned(vec![raw]).await;
        let transport = ReqwestTransport::connect_with_resolver(
            &dev_origin_for(server.addr),
            Arc::new(FixedResolver(vec![IpAddr::from([127, 0, 0, 1])])),
        )
        .await
        .unwrap();
        let error = transport
            .send(send_to(server.addr, "/x"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, TransportError::BodyTooLarge(_)),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn decompressed_body_cap_rejects_gzip_bombs() {
        // Wire size ~2 KB, decompressed size 2 MiB > 1 MiB cap.
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        std::io::Write::write_all(&mut encoder, &[0u8; 2 * 1024 * 1024]).unwrap();
        let compressed = encoder.finish().unwrap();
        let server = spawn_canned(vec![http_response(
            "HTTP/1.1 200 OK",
            &[
                ("content-type", "application/json"),
                ("content-encoding", "gzip"),
            ],
            &compressed,
        )])
        .await;
        let transport = ReqwestTransport::connect_with_resolver(
            &dev_origin_for(server.addr),
            Arc::new(FixedResolver(vec![IpAddr::from([127, 0, 0, 1])])),
        )
        .await
        .unwrap();
        let error = transport
            .send(send_to(server.addr, "/x"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, TransportError::BodyTooLarge(_)),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn slow_dribble_is_cut_by_the_total_timeout() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            read_request_headers(&mut sock).await;
            // Advertise 16 body bytes but deliver one every 40 ms: under the
            // idle timeout, far beyond the total budget.
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 16\r\n\r\n")
                .await;
            for _ in 0..16 {
                let _ = sock.write_all(b"x").await;
                let _ = sock.flush().await;
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });
        let transport = ReqwestTransport::connect_with_limits(
            &dev_origin_for(addr),
            Limits {
                total_timeout: Duration::from_millis(200),
                ..Limits::default()
            },
        )
        .await
        .unwrap();
        let error = transport.send(send_to(addr, "/x")).await.unwrap_err();
        assert!(matches!(error, TransportError::Connection(_)), "{error:?}");
        assert!(
            error.to_string().contains("total timeout"),
            "expected the total budget to fire, got: {error}"
        );
        // 16 × 40 ms = 640 ms; the total budget of this transport is 200 ms.
    }

    #[tokio::test]
    async fn stalled_response_is_cut_by_the_idle_timeout() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            read_request_headers(&mut sock).await;
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 16\r\n\r\n")
                .await;
            // Then silence: the idle gap (150 ms) fires before any total
            // budget that could explain a slow drip.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let transport = ReqwestTransport::connect_with_limits(
            &dev_origin_for(addr),
            Limits {
                idle_timeout: Duration::from_millis(150),
                ..Limits::default()
            },
        )
        .await
        .unwrap();
        let error = transport.send(send_to(addr, "/x")).await.unwrap_err();
        assert!(matches!(error, TransportError::Connection(_)), "{error:?}");
        let message = error.to_string();
        assert!(
            message.contains("timed out") || message.contains("stalled"),
            "expected the idle timeout to fire, got: {message}"
        );
    }

    #[tokio::test]
    async fn request_url_on_a_foreign_origin_is_refused_with_zero_io() {
        let server = spawn_canned(vec![http_response("HTTP/1.1 200 OK", &[], b"{}")]).await;
        let transport = ReqwestTransport::connect_with_resolver(
            &dev_origin_for(server.addr),
            Arc::new(FixedResolver(vec![IpAddr::from([127, 0, 0, 1])])),
        )
        .await
        .unwrap();
        let foreign = OutboundRequest {
            method: "GET",
            url: "http://other-host.test/api/v1/discovery/capabilities".into(),
            bearer: None,
            json_body: None,
        };
        let error = transport.send(foreign).await.unwrap_err();
        assert!(
            matches!(error, TransportError::PolicyRejected(_)),
            "{error:?}"
        );
        assert_eq!(server.connections.load(Ordering::SeqCst), 0);
        // Same host but wrong scheme is equally refused.
        let downgrade = OutboundRequest {
            method: "GET",
            url: format!("https://{}", server.addr),
            bearer: None,
            json_body: None,
        };
        let error = transport.send(downgrade).await.unwrap_err();
        assert!(
            matches!(error, TransportError::PolicyRejected(_)),
            "{error:?}"
        );
    }
}
