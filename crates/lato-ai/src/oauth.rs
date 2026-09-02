use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const OPENAI_DEVICE_USER_CODE_URL: &str =
    "https://auth.openai.com/api/accounts/deviceauth/usercode";
const OPENAI_DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const OPENAI_DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
const OPENAI_DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const KIMI_DEVICE_URL: &str = "https://auth.kimi.com/api/oauth/device_authorization";
const KIMI_TOKEN_URL: &str = "https://auth.kimi.com/api/oauth/token";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthNotice {
    AuthUrl(String),
    DeviceCode {
        code: String,
        verification_url: String,
    },
    Info(String),
    Progress(String),
}

#[async_trait]
pub trait AuthInteraction: Send + Sync {
    async fn notify(&self, notice: AuthNotice);
    async fn redirect_url(&self) -> Result<String, String>;

    fn prefers_local_callback(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OpenAICodexLoginMode {
    #[default]
    Browser,
    DeviceCode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthTokens {
    pub access: String,
    pub refresh: String,
    pub expires: i64,
    pub account_id: Option<String>,
}

pub async fn login_oauth(
    provider: &str,
    interaction: &dyn AuthInteraction,
    client: &reqwest::Client,
) -> Result<OAuthTokens, String> {
    login_oauth_with_mode(provider, OpenAICodexLoginMode::Browser, interaction, client).await
}

pub async fn login_oauth_with_mode(
    provider: &str,
    mode: OpenAICodexLoginMode,
    interaction: &dyn AuthInteraction,
    client: &reqwest::Client,
) -> Result<OAuthTokens, String> {
    match provider {
        "openai-codex" => match mode {
            OpenAICodexLoginMode::Browser => login_openai_codex(interaction, client).await,
            OpenAICodexLoginMode::DeviceCode => {
                login_openai_codex_device(interaction, client).await
            }
        },
        _ if mode == OpenAICodexLoginMode::DeviceCode => {
            Err("device auth is only supported for openai-codex".into())
        }
        "kimi-coding" => login_kimi_coding(interaction, client).await,
        _ => Err("oauth not supported for provider".into()),
    }
}

async fn login_openai_codex(
    interaction: &dyn AuthInteraction,
    client: &reqwest::Client,
) -> Result<OAuthTokens, String> {
    let (verifier, challenge) = pkce_pair();
    let state = random_urlsafe(24);
    let listener =
        if interaction.prefers_local_callback() {
            Some(TcpListener::bind("127.0.0.1:1455").await.map_err(|error| {
                format!("cannot listen for OAuth callback on port 1455: {error}")
            })?)
        } else {
            None
        };
    let mut url = url::Url::parse(OPENAI_AUTHORIZE_URL).unwrap();
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", OPENAI_CLIENT_ID)
        .append_pair("redirect_uri", OPENAI_REDIRECT_URI)
        .append_pair("scope", "openid profile email offline_access")
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", "lato");
    interaction
        .notify(AuthNotice::AuthUrl(url.to_string()))
        .await;
    let redirect = match listener {
        Some(listener) => receive_openai_callback(listener).await?,
        None => interaction.redirect_url().await?,
    };
    let code = validate_openai_redirect(&redirect, &state)?;
    exchange_openai_code(client, &code, &verifier, OPENAI_REDIRECT_URI).await
}

fn validate_openai_redirect(redirect: &str, expected_state: &str) -> Result<String, String> {
    let redirect = url::Url::parse(redirect).map_err(|e| e.to_string())?;
    if redirect
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        != Some(expected_state.to_string())
    {
        return Err("oauth state mismatch".into());
    }
    redirect
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.into_owned())
        .ok_or_else(|| "oauth redirect missing code".to_string())
}

async fn exchange_openai_code(
    client: &reqwest::Client,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthTokens, String> {
    let response = client
        .post(OPENAI_TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", OPENAI_CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    with_openai_account_id(parse_token_response(response).await?)
}

async fn receive_openai_callback(listener: TcpListener) -> Result<String, String> {
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(15 * 60), listener.accept())
        .await
        .map_err(|_| "OAuth callback timed out".to_string())?
        .map_err(|error| error.to_string())?;
    let mut request = Vec::with_capacity(2048);
    let mut chunk = [0_u8; 1024];
    loop {
        let read = socket.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if request.len() > 16 * 1024 {
            return Err("OAuth callback request is too large".into());
        }
    }
    let first_line = std::str::from_utf8(&request)
        .map_err(|_| "OAuth callback was not valid UTF-8".to_string())?
        .lines()
        .next()
        .ok_or_else(|| "OAuth callback request was empty".to_string())?;
    let target = first_line
        .split_ascii_whitespace()
        .nth(1)
        .ok_or_else(|| "OAuth callback request was malformed".to_string())?;
    let redirect = url::Url::parse("http://localhost:1455")
        .unwrap()
        .join(target)
        .map_err(|e| e.to_string())?;
    if redirect.path() != "/auth/callback" {
        let _ = socket
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
            .await;
        return Err("unexpected OAuth callback path".into());
    }
    let body = b"OpenAI Codex login complete. You can close this window.";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    socket.write_all(body).await.map_err(|e| e.to_string())?;
    Ok(redirect.to_string())
}

async fn login_openai_codex_device(
    interaction: &dyn AuthInteraction,
    client: &reqwest::Client,
) -> Result<OAuthTokens, String> {
    let response = client
        .post(OPENAI_DEVICE_USER_CODE_URL)
        .json(&serde_json::json!({"client_id": OPENAI_CLIENT_ID}))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let value: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "OpenAI device authorization failed ({status}): {value}"
        ));
    }
    let device_auth_id = value
        .get("device_auth_id")
        .and_then(|v| v.as_str())
        .ok_or("missing device_auth_id")?;
    let user_code = value
        .get("user_code")
        .and_then(|v| v.as_str())
        .ok_or("missing user_code")?;
    let mut interval = json_u64(&value, "interval").unwrap_or(5).max(1);
    interaction
        .notify(AuthNotice::DeviceCode {
            code: user_code.into(),
            verification_url: OPENAI_DEVICE_VERIFICATION_URL.into(),
        })
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15 * 60);
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let response = client
            .post(OPENAI_DEVICE_TOKEN_URL)
            .json(&serde_json::json!({
                "device_auth_id": device_auth_id,
                "user_code": user_code
            }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let value: serde_json::Value = response.json().await.unwrap_or_default();
        if status.is_success() {
            let code = value
                .get("authorization_code")
                .and_then(|v| v.as_str())
                .ok_or("missing authorization_code")?;
            let verifier = value
                .get("code_verifier")
                .and_then(|v| v.as_str())
                .ok_or("missing code_verifier")?;
            return exchange_openai_code(client, code, verifier, OPENAI_DEVICE_REDIRECT_URI).await;
        }
        let error = oauth_error_code(&value);
        if error == "slow_down" {
            interval += 5;
        } else if !matches!(
            error.as_str(),
            "authorization_pending" | "deviceauth_authorization_pending"
        ) && !matches!(status.as_u16(), 403 | 404)
        {
            return Err(format!("OpenAI device token failed ({status}): {value}"));
        }
        interaction.notify(AuthNotice::Progress(error)).await;
    }
    Err("OpenAI device code expired".into())
}

fn json_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    })
}

fn oauth_error_code(value: &serde_json::Value) -> String {
    value
        .get("error")
        .and_then(|value| value.as_str().or_else(|| value.get("code")?.as_str()))
        .unwrap_or("authorization_pending")
        .to_string()
}

async fn login_kimi_coding(
    interaction: &dyn AuthInteraction,
    client: &reqwest::Client,
) -> Result<OAuthTokens, String> {
    let response = client
        .post(KIMI_DEVICE_URL)
        .json(
            &serde_json::json!({"client_id":"kimi-coding","scope":"openid profile offline_access"}),
        )
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let value: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("device authorization failed: {value}"));
    }
    let device_code = value
        .get("device_code")
        .and_then(|v| v.as_str())
        .ok_or("missing device_code")?;
    let user_code = value
        .get("user_code")
        .and_then(|v| v.as_str())
        .ok_or("missing user_code")?;
    let verification_url = value
        .get("verification_uri_complete")
        .or_else(|| value.get("verification_uri"))
        .and_then(|v| v.as_str())
        .ok_or("missing verification URL")?;
    let interval = value
        .get("interval")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .max(1);
    let expires_in = value
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(900);
    interaction
        .notify(AuthNotice::DeviceCode {
            code: user_code.into(),
            verification_url: verification_url.into(),
        })
        .await;
    let attempts = (expires_in / interval).max(1);
    for _ in 0..attempts {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let response = client
            .post(KIMI_TOKEN_URL)
            .json(&serde_json::json!({
                "grant_type":"urn:ietf:params:oauth:grant-type:device_code",
                "client_id":"kimi-coding",
                "device_code":device_code
            }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status().is_success() {
            return parse_token_response(response).await;
        }
        let status = response.status();
        let value: serde_json::Value = response.json().await.unwrap_or_default();
        let code = value.get("error").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(code, "authorization_pending" | "slow_down") {
            return Err(format!("device token failed ({status}): {value}"));
        }
        interaction.notify(AuthNotice::Progress(code.into())).await;
    }
    Err("device code expired".into())
}

pub async fn refresh_oauth_token(
    provider: &str,
    refresh: &str,
    client: &reqwest::Client,
) -> Result<OAuthTokens, String> {
    let (url, client_id) = match provider {
        "openai-codex" => (OPENAI_TOKEN_URL, OPENAI_CLIENT_ID),
        "kimi-coding" => (KIMI_TOKEN_URL, "kimi-coding"),
        _ => return Err("oauth not supported for provider".into()),
    };
    let response = client
        .post(url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let tokens = refresh_token_provider(provider, parse_token_response(response).await?)?;
    Ok(tokens)
}

fn with_openai_account_id(mut tokens: OAuthTokens) -> Result<OAuthTokens, String> {
    tokens.account_id = Some(extract_chatgpt_account_id(&tokens.access)?);
    Ok(tokens)
}

fn refresh_token_provider(provider: &str, tokens: OAuthTokens) -> Result<OAuthTokens, String> {
    if provider == "openai-codex" {
        with_openai_account_id(tokens)
    } else {
        Ok(tokens)
    }
}

pub fn extract_chatgpt_account_id(access_token: &str) -> Result<String, String> {
    let payload = access_token
        .split('.')
        .nth(1)
        .ok_or_else(|| "OpenAI Codex access token is not a JWT".to_string())?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| "OpenAI Codex access token has an invalid JWT payload".to_string())?;
    let value: serde_json::Value = serde_json::from_slice(&decoded)
        .map_err(|_| "OpenAI Codex access token has an invalid JWT payload".to_string())?;
    value
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "OpenAI Codex access token is missing the ChatGPT account ID".to_string())
}

async fn parse_token_response(response: reqwest::Response) -> Result<OAuthTokens, String> {
    let status = response.status();
    let value: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("token exchange failed ({status}): {value}"));
    }
    let access = value
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("missing access_token")?
        .to_string();
    let refresh = value
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let expires_in = value
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3600);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    Ok(OAuthTokens {
        access,
        refresh,
        expires: now + expires_in * 1000,
        account_id: None,
    })
}

fn pkce_pair() -> (String, String) {
    let verifier = random_urlsafe(48);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn random_urlsafe(bytes: usize) -> String {
    let mut data = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut data);
    URL_SAFE_NO_PAD.encode(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_chatgpt_account_id_from_access_jwt() {
        let payload = URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-7"}}"#);
        let token = format!("header.{payload}.signature");
        assert_eq!(extract_chatgpt_account_id(&token).unwrap(), "acct-7");
    }

    #[test]
    fn c1_pkce_is_s256_and_has_sufficient_entropy() {
        let (verifier, challenge) = pkce_pair();
        assert!(verifier.len() >= 43);
        assert_eq!(
            challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
        );
    }

    #[tokio::test]
    async fn c1_4_oauth_dispatch_rejects_other_providers_before_network() {
        struct Noop;
        #[async_trait]
        impl AuthInteraction for Noop {
            async fn notify(&self, _: AuthNotice) {}
            async fn redirect_url(&self) -> Result<String, String> {
                Err("unused".into())
            }
        }
        assert!(
            login_oauth("xai", &Noop, &reqwest::Client::new())
                .await
                .unwrap_err()
                .contains("not supported")
        );
    }
}
