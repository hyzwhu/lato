use crate::{Credential, CredentialStore};
use serde_json::json;

#[derive(Clone, Debug, Default)]
pub struct Auth {
    pub api_key: Option<String>,
    pub headers: Vec<(String, String)>,
    pub base_url: Option<String>,
}

pub async fn get_auth(
    store: &CredentialStore,
    provider_id: &str,
    env: &dyn Fn(&str) -> Option<String>,
    override_key: Option<String>,
) -> Option<Auth> {
    if let Some(k) = override_key {
        return Some(Auth {
            api_key: Some(k),
            ..Default::default()
        });
    }
    if let Some(c) = store.get(provider_id) {
        return match c {
            Credential::Oauth { access, .. } => Some(Auth {
                api_key: Some(access),
                ..Default::default()
            }),
            Credential::ApiKey { key } => Some(Auth {
                api_key: Some(resolve_key(&key, env)),
                ..Default::default()
            }),
        };
    }
    for name in env_names(provider_id) {
        if let Some(v) = env(name) {
            if provider_id == "anthropic" && *name == "ANTHROPIC_AUTH_TOKEN" {
                return Some(Auth {
                    api_key: None,
                    headers: vec![("authorization".into(), format!("Bearer {v}"))],
                    ..Default::default()
                });
            }
            return Some(Auth {
                api_key: Some(v),
                ..Default::default()
            });
        }
    }
    None
}

fn resolve_key(raw: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
    if let Some(name) = raw.strip_prefix('$') {
        env(name).unwrap_or_default()
    } else if let Some(command) = raw.strip_prefix('!') {
        run_secret_command(command).unwrap_or_default()
    } else {
        raw.to_string()
    }
}

fn run_secret_command(command: &str) -> Option<String> {
    #[cfg(windows)]
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", command])
        .output()
        .ok()?;
    #[cfg(not(windows))]
    let output = std::process::Command::new("sh")
        .args(["-c", command])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn oauth_allowed(provider_id: &str) -> bool {
    matches!(provider_id, "kimi-coding" | "openai-codex")
}

pub fn api_key_login_allowed(provider_id: &str) -> bool {
    crate::preset(provider_id).is_some() && provider_id != "openai-codex"
}

pub fn store_oauth(
    store: &mut CredentialStore,
    provider_id: &str,
    access: &str,
    refresh: &str,
    expires: i64,
) -> std::io::Result<()> {
    if !oauth_allowed(provider_id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "oauth not supported for provider",
        ));
    }
    store.modify(|m| {
        m.insert(
            provider_id.into(),
            json!({"type":"oauth","access":access,"refresh":refresh,"expires":expires}),
        );
    })
}

pub async fn refresh_oauth_after_401<F>(
    store: &mut CredentialStore,
    provider_id: &str,
    refresh_fn: F,
) -> Result<Auth, String>
where
    F: FnOnce(&str) -> Result<(String, String, i64), String>,
{
    let Some(Credential::Oauth { refresh, .. }) = store.get(provider_id) else {
        return Err("stored credential is not oauth".into());
    };
    let (access, new_refresh, expires) = refresh_fn(&refresh)?;
    store_oauth(store, provider_id, &access, &new_refresh, expires).map_err(|e| e.to_string())?;
    Ok(Auth {
        api_key: Some(access),
        headers: vec![],
        base_url: None,
    })
}

pub fn env_names(provider_id: &str) -> &'static [&'static str] {
    crate::preset(provider_id)
        .map(|preset| preset.env)
        .unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn a4_1_env_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::open(dir.path()).unwrap();
        let auth = get_auth(
            &store,
            "openai",
            &|k| (k == "OPENAI_API_KEY").then(|| "sk-env".into()),
            None,
        )
        .await;
        assert_eq!(auth.unwrap().api_key.as_deref(), Some("sk-env"));
    }
    #[tokio::test]
    async fn a4_2_store_beats_env() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CredentialStore::open(dir.path()).unwrap();
        store
            .modify(|m| {
                m.insert("openai".into(), json!({"type":"api_key","key":"sk-disk"}));
            })
            .unwrap();
        let auth = get_auth(&store, "openai", &|_| Some("sk-env".into()), None).await;
        assert_eq!(auth.unwrap().api_key.as_deref(), Some("sk-disk"));
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn credential_command_value_resolves_trimmed_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CredentialStore::open(dir.path()).unwrap();
        store
            .modify(|m| {
                m.insert(
                    "openai".into(),
                    json!({"type":"api_key","key":"!printf command-secret"}),
                );
            })
            .unwrap();
        let auth = get_auth(&store, "openai", &|_| None, None).await.unwrap();
        assert_eq!(auth.api_key.as_deref(), Some("command-secret"));
    }
    #[tokio::test]
    async fn a4_3_oauth_refresh_fail_does_not_use_env() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CredentialStore::open(dir.path()).unwrap();
        store
            .modify(|m| {
                m.insert(
                    "kimi-coding".into(),
                    json!({"type":"oauth","access":"a","refresh":"r","expires":0}),
                );
            })
            .unwrap();
        let auth = get_auth(&store, "kimi-coding", &|_| Some("env-key".into()), None).await;
        assert_eq!(auth.unwrap().api_key.as_deref(), Some("a"));
    }
    #[test]
    fn a4_4_and_g1_oauth_not_allowed() {
        assert!(!oauth_allowed("xai"));
        assert!(!oauth_allowed("anthropic"));
        assert!(!oauth_allowed("openrouter"));
        assert!(oauth_allowed("kimi-coding"));
        assert!(oauth_allowed("openai-codex"));
    }

    #[test]
    fn b1_5_cloud_specials_not_api_key_login_allowed() {
        assert!(!api_key_login_allowed("amazon-bedrock"));
        assert!(!api_key_login_allowed("google-vertex"));
        assert!(!api_key_login_allowed("cloudflare-workers-ai"));
        assert!(api_key_login_allowed("openai"));
    }

    #[tokio::test]
    async fn b1_7_anthropic_auth_token_env_is_bearer() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::open(dir.path()).unwrap();
        let auth = get_auth(
            &store,
            "anthropic",
            &|k| (k == "ANTHROPIC_AUTH_TOKEN").then(|| "token".into()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(auth.api_key, None);
        assert_eq!(
            auth.headers,
            vec![("authorization".into(), "Bearer token".into())]
        );
    }

    #[tokio::test]
    async fn c1_1_kimi_oauth_persists_and_resolves_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CredentialStore::open(dir.path()).unwrap();
        store_oauth(&mut store, "kimi-coding", "access", "refresh", 123).unwrap();
        let reopened = CredentialStore::open(dir.path()).unwrap();
        let auth = get_auth(&reopened, "kimi-coding", &|_| None, None)
            .await
            .unwrap();
        assert_eq!(auth.api_key.as_deref(), Some("access"));
    }

    #[tokio::test]
    async fn c1_2_openai_codex_oauth_persists_with_separate_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CredentialStore::open(dir.path()).unwrap();
        store_oauth(&mut store, "openai-codex", "access", "refresh", 123).unwrap();
        assert!(store.get("openai-codex").is_some());
        assert!(store.get("openai").is_none());
        let reopened = CredentialStore::open(dir.path()).unwrap();
        let auth = get_auth(&reopened, "openai-codex", &|_| None, None)
            .await
            .unwrap();
        assert_eq!(auth.api_key.as_deref(), Some("access"));
    }

    #[test]
    fn c1_4_oauth_rejected_outside_two_providers() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CredentialStore::open(dir.path()).unwrap();
        assert!(store_oauth(&mut store, "xai", "a", "r", 0).is_err());
    }

    #[tokio::test]
    async fn c1_5_oauth_401_refresh_success_once_no_env_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CredentialStore::open(dir.path()).unwrap();
        store_oauth(&mut store, "kimi-coding", "old", "refresh", 0).unwrap();
        let auth = refresh_oauth_after_401(&mut store, "kimi-coding", |r| {
            assert_eq!(r, "refresh");
            Ok(("new-access".into(), "new-refresh".into(), 999))
        })
        .await
        .unwrap();
        assert_eq!(auth.api_key.as_deref(), Some("new-access"));
        let resolved = get_auth(&store, "kimi-coding", &|_| Some("env".into()), None)
            .await
            .unwrap();
        assert_eq!(resolved.api_key.as_deref(), Some("new-access"));
    }

    #[tokio::test]
    async fn c1_5_oauth_401_refresh_failure_does_not_use_env() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CredentialStore::open(dir.path()).unwrap();
        store_oauth(&mut store, "kimi-coding", "old", "refresh", 0).unwrap();
        assert!(
            refresh_oauth_after_401(&mut store, "kimi-coding", |_| Err("nope".into()))
                .await
                .is_err()
        );
        let resolved = get_auth(&store, "kimi-coding", &|_| Some("env".into()), None)
            .await
            .unwrap();
        assert_eq!(resolved.api_key.as_deref(), Some("old"));
    }
}
