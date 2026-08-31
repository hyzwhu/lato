use std::net::{IpAddr, ToSocketAddrs};

pub async fn web_fetch(url: &str, max_bytes: usize) -> Result<String, String> {
    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("only http and https URLs are allowed".into());
    }
    let host = parsed.host_str().ok_or("URL has no host")?;
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return Err("SSRF policy denied local host".into());
    }
    let port = parsed.port_or_known_default().ok_or("URL has no port")?;
    let addresses: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .collect();
    if addresses.is_empty() || addresses.iter().any(|addr| denied_ip(addr.ip())) {
        return Err("SSRF policy denied private address".into());
    }
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| e.to_string())?
        .get(parsed)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    let end = bytes.len().min(max_bytes);
    Ok(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

fn denied_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.octets()[0] == 0
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn web_fetch_ssrf_rejects_loopback_before_request() {
        assert!(
            web_fetch("http://127.0.0.1:1234/secret", 100)
                .await
                .unwrap_err()
                .contains("SSRF")
        );
        assert!(
            web_fetch("http://localhost/secret", 100)
                .await
                .unwrap_err()
                .contains("SSRF")
        );
    }
}
