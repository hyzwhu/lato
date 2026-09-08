use std::{
    io,
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{Client, redirect::Policy};
use tokio::net::lookup_host;
use url::Url;

use super::{
    HookEventEnvelope, HookRunContext, HookRunError, HookSpec, MAX_PAYLOAD_BYTES,
    MAX_RUNNER_OUTPUT_BYTES, RawHookRun,
};

#[async_trait]
pub trait HookDnsResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemHookDnsResolver;

#[async_trait]
impl HookDnsResolver for SystemHookDnsResolver {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(lookup_host((host, port)).await?.collect())
    }
}

pub fn build_hook_http_client() -> Result<Client, HookRunError> {
    Client::builder()
        .redirect(Policy::none())
        .build()
        .map_err(|_| HookRunError::Http)
}

pub async fn validate_hook_url(
    url: &Url,
    resolver: &dyn HookDnsResolver,
) -> Result<(), HookRunError> {
    if url.scheme() != "https" || url.username() != "" || url.password().is_some() {
        return Err(HookRunError::UnsafeUrl);
    }
    let host = url.host_str().ok_or(HookRunError::UnsafeUrl)?;
    let port = url.port_or_known_default().ok_or(HookRunError::UnsafeUrl)?;
    let addresses = resolver
        .resolve(host, port)
        .await
        .map_err(|_| HookRunError::UnsafeUrl)?;
    if addresses.is_empty() || addresses.iter().any(|address| blocked(address.ip())) {
        return Err(HookRunError::UnsafeUrl);
    }
    Ok(())
}

pub async fn run_http_hook(
    spec: &HookSpec,
    envelope: &HookEventEnvelope,
    context: &HookRunContext<'_>,
    client: &Client,
    resolver: &dyn HookDnsResolver,
) -> Result<RawHookRun, HookRunError> {
    let payload = serde_json::to_vec(envelope).map_err(|_| HookRunError::PayloadTooLarge)?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(HookRunError::PayloadTooLarge);
    }
    let configured = spec
        .url
        .as_deref()
        .ok_or(HookRunError::InvalidConfiguration)?;
    let expanded = expand_url(configured, spec, context);
    let url = Url::parse(&expanded).map_err(|_| HookRunError::UnsafeUrl)?;
    let started = Instant::now();
    let timeout = Duration::from_millis(spec.timeout_ms);
    let operation = async {
        validate_hook_url(&url, resolver).await?;
        let response = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload)
            .send()
            .await
            .map_err(|_| HookRunError::Http)?;
        let status = response.status();
        if status.is_redirection() || !status.is_success() {
            return Err(HookRunError::Http);
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| HookRunError::Http)?;
            if body.len().saturating_add(chunk.len()) > MAX_RUNNER_OUTPUT_BYTES {
                return Err(HookRunError::OutputOverflow);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(RawHookRun {
            stdout: String::from_utf8_lossy(&body).into_owned(),
            stderr_preview: String::new(),
            exit_code: Some(0),
            elapsed: started.elapsed(),
            truncated: false,
        })
    };
    tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => Err(HookRunError::Cancelled),
        _ = tokio::time::sleep(timeout) => Err(HookRunError::Timeout { timeout_ms: spec.timeout_ms }),
        result = operation => result,
    }
}

fn expand_url(value: &str, spec: &HookSpec, context: &HookRunContext<'_>) -> String {
    let mut expanded = value.to_owned();
    let identities = [
        ("LATO_HOOK_EVENT", spec.event.as_str()),
        ("LATO_HOOK_NAME", spec.id.as_str()),
        ("LATO_SESSION_ID", context.session_id),
        (
            "LATO_WORKSPACE_ROOT",
            context.workspace_root.to_str().unwrap_or(""),
        ),
        (
            "CLAUDE_PROJECT_DIR",
            context.workspace_root.to_str().unwrap_or(""),
        ),
    ];
    for (key, value) in spec
        .extra_env
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .chain(identities)
    {
        expanded = expanded.replace(&format!("${{{key}}}"), value);
    }
    expanded
}

fn blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            if ip.is_loopback() {
                return false;
            }
            let octets = ip.octets();
            ip.is_unspecified()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_multicast()
                || ip.is_broadcast()
                || octets[0] == 0
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                || (octets[0] == 198 && matches!(octets[1], 18 | 19))
        }
        IpAddr::V6(ip) => {
            if ip.is_loopback() {
                return false;
            }
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return blocked(IpAddr::V4(mapped));
            }
            ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
        }
    }
}
