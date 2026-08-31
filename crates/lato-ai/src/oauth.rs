use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::time::Duration;

const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthTokens {
    pub access: String,
    pub refresh: String,
    pub expires: i64,
}

pub async fn login_oauth(
    provider: &str,
    interaction: &dyn AuthInteraction,
    client: &reqwest::Client,
) -> Result<OAuthTokens, String> {
    match provider {
        "openai-codex" => login_openai_codex(interaction, client).await,
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
    let mut url = url::Url::parse(OPENAI_AUTHORIZE_URL).unwrap();
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", OPENAI_CLIENT_ID)
        .append_pair("redirect_uri", OPENAI_REDIRECT_URI)
        .append_pair("scope", "openid profile email offline_access")
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("codex_cli_simplified_flow", "true");
    interaction
        .notify(AuthNotice::AuthUrl(url.to_string()))
        .await;
    let redirect = interaction.redirect_url().await?;
    let redirect = url::Url::parse(&redirect).map_err(|e| e.to_string())?;
    if redirect
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        != Some(state)
    {
        return Err("oauth state mismatch".into());
    }
    let code = redirect
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.into_owned())
        .ok_or("oauth redirect missing code")?;
    let response = client
        .post(OPENAI_TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", OPENAI_CLIENT_ID),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("redirect_uri", OPENAI_REDIRECT_URI),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    parse_token_response(response).await
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
    parse_token_response(response).await
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
