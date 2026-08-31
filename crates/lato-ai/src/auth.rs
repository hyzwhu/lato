use crate::{Credential, CredentialStore};

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
    } else {
        raw.to_string()
    }
}

pub fn oauth_allowed(provider_id: &str) -> bool {
    matches!(provider_id, "kimi-coding" | "openai-codex")
}

pub fn env_names(provider_id: &str) -> &'static [&'static str] {
    match provider_id {
        "openai" => &["OPENAI_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        "kimi-coding" => &["KIMI_API_KEY"],
        "anthropic" => &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"],
        _ => &[],
    }
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
}
