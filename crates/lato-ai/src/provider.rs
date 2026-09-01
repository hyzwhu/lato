use crate::{CustomModel, ModelApi};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const REMOTE_CATALOG_REFRESH_INTERVAL_MS: i64 = 4 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub base_url: &'static str,
    pub api: ModelApi,
    pub env: &'static [&'static str],
    pub remote_catalog: bool,
}

pub const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        id: "minimax",
        name: "MiniMax",
        base_url: "https://api.minimax.io/anthropic",
        api: ModelApi::AnthropicMessages,
        env: &["MINIMAX_API_KEY"],
        remote_catalog: true,
    },
    ProviderSpec {
        id: "minimax-cn",
        name: "MiniMax CN",
        base_url: "https://api.minimaxi.com/anthropic",
        api: ModelApi::AnthropicMessages,
        env: &["MINIMAX_CN_API_KEY"],
        remote_catalog: true,
    },
    ProviderSpec {
        id: "zai",
        name: "Z.AI",
        base_url: "https://api.z.ai/api/coding/paas/v4",
        api: ModelApi::OpenaiCompletions,
        env: &["ZAI_API_KEY"],
        remote_catalog: true,
    },
    ProviderSpec {
        id: "zai-coding-cn",
        name: "Z.AI Coding CN (智谱)",
        base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
        api: ModelApi::OpenaiCompletions,
        env: &["ZAI_CODING_CN_API_KEY"],
        remote_catalog: true,
    },
    // SenseNova is not present in the reference provider registry. Keep it explicit and static
    // instead of pretending that the reference catalog defines its protocol.
    ProviderSpec {
        id: "sensenova",
        name: "SenseTime SenseNova",
        base_url: "https://api.sensenova.cn/compatible-mode/v1",
        api: ModelApi::OpenaiCompletions,
        env: &["SENSENOVA_API_KEY"],
        remote_catalog: false,
    },
];

pub fn provider_spec(id: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|provider| provider.id == id)
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProviderModelsEntry {
    pub models: Vec<CustomModel>,
    #[serde(default)]
    pub checked_at: i64,
    #[serde(default)]
    pub last_modified: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct ModelsStoreDocument {
    #[serde(default)]
    providers: BTreeMap<String, ProviderModelsEntry>,
}

pub struct ProviderModelsStore {
    path: PathBuf,
}

impl ProviderModelsStore {
    pub fn open(home: &Path) -> Self {
        Self {
            path: home.join("models-store.json"),
        }
    }

    pub fn read(&self, provider: &str) -> Result<Option<ProviderModelsEntry>, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)
            .map_err(|error| error.to_string())?;
        file.lock_shared().map_err(|error| error.to_string())?;
        let document = read_document(&file)?;
        let result = document.providers.get(provider).cloned();
        let _ = file.unlock();
        Ok(result)
    }

    pub fn write(&self, provider: &str, entry: ProviderModelsEntry) -> Result<(), String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)
            .map_err(|error| error.to_string())?;
        file.lock_exclusive().map_err(|error| error.to_string())?;
        let mut document = read_document(&file)?;
        document.providers.insert(provider.to_string(), entry);
        let bytes = serde_json::to_vec_pretty(&document).map_err(|error| error.to_string())?;
        file.set_len(0).map_err(|error| error.to_string())?;
        std::io::Write::write_all(&mut &file, &bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        let _ = file.unlock();
        Ok(())
    }
}

fn read_document(mut file: &std::fs::File) -> Result<ModelsStoreDocument, String> {
    use std::io::{Read, Seek};
    file.rewind().map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.is_empty() {
        return Ok(ModelsStoreDocument::default());
    }
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

pub async fn refresh_remote_provider_catalog(
    spec: &ProviderSpec,
    store: &ProviderModelsStore,
    catalog_base_url: &str,
    force: bool,
) -> Result<Vec<CustomModel>, String> {
    let stored = store.read(spec.id)?;
    let now = now_ms();
    if !force
        && stored.as_ref().is_some_and(|entry| {
            entry.checked_at > 0 && now - entry.checked_at < REMOTE_CATALOG_REFRESH_INTERVAL_MS
        })
    {
        return Ok(stored.unwrap().models);
    }
    let url = format!(
        "{}/api/models/providers/{}",
        catalog_base_url.trim_end_matches('/'),
        spec.id
    );
    let client = crate::http_client_for_url(&url);
    let mut request = client
        .get(url)
        .header("accept", "application/json")
        .header("user-agent", "lato/0.1.0");
    if let Some(etag) = stored.as_ref().and_then(|entry| {
        (!entry.models.is_empty())
            .then_some(entry.etag.as_ref())
            .flatten()
    }) {
        request = request.header("if-none-match", etag);
    }
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    if status.as_u16() == 304 {
        let mut entry = stored.ok_or("catalog returned 304 without cached models")?;
        entry.checked_at = now;
        store.write(spec.id, entry.clone())?;
        return Ok(entry.models);
    }
    if matches!(status.as_u16(), 404 | 501) {
        let entry = ProviderModelsEntry {
            checked_at: now,
            ..stored.unwrap_or_default()
        };
        store.write(spec.id, entry.clone())?;
        return Ok(entry.models);
    }
    if !status.is_success() {
        let mut entry = stored.unwrap_or_default();
        entry.checked_at = now;
        store.write(spec.id, entry)?;
        return Err(format!(
            "model catalog request failed for {}: {status}",
            spec.id
        ));
    }
    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let last_modified = response
        .headers()
        .get("last-modified")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| httpdate::parse_http_date(value).ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let value: serde_json::Value = response.json().await.map_err(|error| error.to_string())?;
    let models = parse_remote_catalog(spec, &value)?;
    let entry = ProviderModelsEntry {
        models: models.clone(),
        checked_at: now,
        last_modified,
        etag,
    };
    store.write(spec.id, entry)?;
    Ok(models)
}

fn parse_remote_catalog(
    spec: &ProviderSpec,
    value: &serde_json::Value,
) -> Result<Vec<CustomModel>, String> {
    let entries: Vec<&serde_json::Value> = if let Some(entries) = value.as_array() {
        entries.iter().collect()
    } else if let Some(entries) = value.get("models").and_then(serde_json::Value::as_array) {
        entries.iter().collect()
    } else if let Some(entries) = value.as_object() {
        entries.values().collect()
    } else {
        return Err(format!("invalid model catalog for provider {}", spec.id));
    };
    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?;
            let api = entry
                .get("api")
                .and_then(|value| value.as_str())
                .and_then(parse_api)
                .unwrap_or(spec.api);
            let base_url = entry
                .get("baseUrl")
                .or_else(|| entry.get("base_url"))
                .and_then(|value| value.as_str())
                .unwrap_or(spec.base_url);
            Some(CustomModel {
                provider: spec.id.to_string(),
                id: id.to_string(),
                api,
                base_url: base_url.to_string(),
                env: spec
                    .env
                    .first()
                    .copied()
                    .unwrap_or("LATO_API_KEY")
                    .to_string(),
            })
        })
        .collect())
}

fn parse_api(value: &str) -> Option<ModelApi> {
    match value {
        "openai-completions" => Some(ModelApi::OpenaiCompletions),
        "openai-responses" => Some(ModelApi::OpenaiResponses),
        "anthropic-messages" => Some(ModelApi::AnthropicMessages),
        _ => None,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_provider_definitions_match_typescript_factories() {
        let minimax = provider_spec("minimax-cn").unwrap();
        assert_eq!(minimax.base_url, "https://api.minimaxi.com/anthropic");
        assert_eq!(minimax.api, ModelApi::AnthropicMessages);
        assert_eq!(minimax.env, &["MINIMAX_CN_API_KEY"]);
        let zai = provider_spec("zai-coding-cn").unwrap();
        assert_eq!(zai.base_url, "https://open.bigmodel.cn/api/coding/paas/v4");
        assert_eq!(zai.api, ModelApi::OpenaiCompletions);
        assert_eq!(zai.env, &["ZAI_CODING_CN_API_KEY"]);
        assert!(!provider_spec("sensenova").unwrap().remote_catalog);
    }

    #[tokio::test]
    async fn remote_catalog_is_published_then_restored_within_freshness_window() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 8192];
            let count = socket.read(&mut request).unwrap();
            assert!(
                String::from_utf8_lossy(&request[..count])
                    .starts_with("GET /api/models/providers/minimax-cn")
            );
            let body = r#"{"models":[{"id":"MiniMax-M2.5","api":"anthropic-messages"}]}"#;
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nETag: \"v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let home = tempfile::tempdir().unwrap();
        let store = ProviderModelsStore::open(home.path());
        let spec = provider_spec("minimax-cn").unwrap();
        let first =
            refresh_remote_provider_catalog(spec, &store, &format!("http://{address}"), false)
                .await
                .unwrap();
        assert_eq!(first[0].id, "MiniMax-M2.5");
        assert_eq!(first[0].api, ModelApi::AnthropicMessages);
        let restored = refresh_remote_provider_catalog(spec, &store, "http://127.0.0.1:1", false)
            .await
            .unwrap();
        assert_eq!(restored[0].id, "MiniMax-M2.5");
        assert_eq!(
            store.read("minimax-cn").unwrap().unwrap().etag.as_deref(),
            Some("\"v1\"")
        );
    }
}
