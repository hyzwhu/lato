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
pub enum RemoteCatalogRefreshPolicy {
    Cached,
    Authoritative { attempts: usize },
}

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
    // SenseNova is not present in the reference provider registry. This explicit compatibility
    // definition follows the provider's OpenAI SDK example and vendor model-list endpoint.
    ProviderSpec {
        id: "sensenova",
        name: "SenseTime SenseNova",
        base_url: "https://token.sensenova.cn/v1",
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

    pub fn write_authoritative(
        &self,
        provider: &str,
        entry: ProviderModelsEntry,
    ) -> Result<(), String> {
        use std::io::{Read, Seek};

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)
            .map_err(|error| format!("open model cache for {provider}: {error}"))?;
        file.lock_exclusive()
            .map_err(|error| format!("lock model cache for {provider}: {error}"))?;
        let mut document = match read_document(&file) {
            Ok(document) => document,
            Err(_) => {
                file.rewind()
                    .map_err(|error| format!("rewind corrupt model cache: {error}"))?;
                let mut corrupt_bytes = Vec::new();
                file.read_to_end(&mut corrupt_bytes)
                    .map_err(|error| format!("read corrupt model cache: {error}"))?;
                write_unique_store_file(&self.path, "corrupt", &corrupt_bytes)?;
                ModelsStoreDocument::default()
            }
        };
        document.providers.insert(provider.to_string(), entry);
        let bytes = serde_json::to_vec_pretty(&document)
            .map_err(|error| format!("serialize model cache for {provider}: {error}"))?;
        let temporary_path = write_unique_store_file(&self.path, "tmp", &bytes)?;
        std::fs::rename(&temporary_path, &self.path).map_err(|error| {
            let _ = std::fs::remove_file(&temporary_path);
            format!("replace model cache for {provider}: {error}")
        })?;
        let _ = file.unlock();
        Ok(())
    }
}

fn write_unique_store_file(path: &Path, kind: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    use std::io::Write;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("models-store.json");
    for sequence in 0..100 {
        let suffix = if sequence == 0 {
            String::new()
        } else {
            format!("-{sequence}")
        };
        let candidate = path.with_file_name(format!(
            "{file_name}.{kind}-{}-{pid}{suffix}",
            now_ms(),
            pid = std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                let result = (|| {
                    set_private_file_permissions(&file)?;
                    file.write_all(bytes)
                        .map_err(|error| format!("write {kind} model cache file: {error}"))?;
                    file.sync_all()
                        .map_err(|error| format!("sync {kind} model cache file: {error}"))
                })();
                if let Err(error) = result {
                    let _ = std::fs::remove_file(&candidate);
                    return Err(error);
                }
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create {kind} model cache file: {error}")),
        }
    }
    Err(format!("could not allocate unique {kind} model cache file"))
}

fn set_private_file_permissions(file: &std::fs::File) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("set private model cache permissions: {error}"))?;
    }
    Ok(())
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
    refresh_remote_provider_catalog_with_policy(
        spec,
        store,
        catalog_base_url,
        force,
        RemoteCatalogRefreshPolicy::Cached,
    )
    .await
}

pub async fn refresh_remote_provider_catalog_with_policy(
    spec: &ProviderSpec,
    store: &ProviderModelsStore,
    catalog_base_url: &str,
    force: bool,
    policy: RemoteCatalogRefreshPolicy,
) -> Result<Vec<CustomModel>, String> {
    let stored = match policy {
        RemoteCatalogRefreshPolicy::Cached => store.read(spec.id)?,
        RemoteCatalogRefreshPolicy::Authoritative { .. } => None,
    };
    let now = now_ms();
    if policy == RemoteCatalogRefreshPolicy::Cached
        && !force
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
    let attempts = match policy {
        RemoteCatalogRefreshPolicy::Cached => 1,
        RemoteCatalogRefreshPolicy::Authoritative { attempts } => attempts.max(1),
    };
    let mut last_error = String::new();

    for attempt in 1..=attempts {
        let mut request = client
            .get(&url)
            .header("accept", "application/json")
            .header("user-agent", "lato/0.1.0");
        if policy == RemoteCatalogRefreshPolicy::Cached
            && let Some(etag) = stored.as_ref().and_then(|entry| {
                (!entry.models.is_empty())
                    .then_some(entry.etag.as_ref())
                    .flatten()
            })
        {
            request = request.header("if-none-match", etag);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                last_error = format!("transport error: {error}");
                if attempt < attempts {
                    tokio::time::sleep(std::time::Duration::from_millis(100 * attempt as u64))
                        .await;
                }
                continue;
            }
        };
        let status = response.status();
        if status.as_u16() == 304 && policy == RemoteCatalogRefreshPolicy::Cached {
            let mut entry = stored
                .clone()
                .ok_or("catalog returned 304 without cached models")?;
            entry.checked_at = now;
            store.write(spec.id, entry.clone())?;
            return Ok(entry.models);
        }
        if matches!(status.as_u16(), 404 | 501) && policy == RemoteCatalogRefreshPolicy::Cached {
            let entry = ProviderModelsEntry {
                checked_at: now,
                ..stored.clone().unwrap_or_default()
            };
            store.write(spec.id, entry.clone())?;
            return Ok(entry.models);
        }
        if !status.is_success() {
            last_error = format!("HTTP status {status}");
            if policy == RemoteCatalogRefreshPolicy::Cached {
                let mut entry = stored.clone().unwrap_or_default();
                entry.checked_at = now;
                store.write(spec.id, entry)?;
            }
            if attempt < attempts {
                tokio::time::sleep(std::time::Duration::from_millis(100 * attempt as u64)).await;
            }
            continue;
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
        let bytes = match response.bytes().await {
            Ok(bytes) if !bytes.iter().all(u8::is_ascii_whitespace) => bytes,
            Ok(_) => {
                last_error = format!("empty response body (status {status})");
                if attempt < attempts {
                    tokio::time::sleep(std::time::Duration::from_millis(100 * attempt as u64))
                        .await;
                }
                continue;
            }
            Err(error) => {
                last_error = format!("response body error (status {status}): {error}");
                if attempt < attempts {
                    tokio::time::sleep(std::time::Duration::from_millis(100 * attempt as u64))
                        .await;
                }
                continue;
            }
        };
        let value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(error) => {
                last_error = format!(
                    "invalid JSON response (status {status}): {error}; body: {}",
                    sanitized_response_preview(&bytes)
                );
                if attempt < attempts {
                    tokio::time::sleep(std::time::Duration::from_millis(100 * attempt as u64))
                        .await;
                }
                continue;
            }
        };
        let models = match parse_remote_catalog(spec, &value) {
            Ok(models) if !models.is_empty() => models,
            Ok(_) => {
                last_error = "catalog contained no usable models".to_string();
                if attempt < attempts {
                    tokio::time::sleep(std::time::Duration::from_millis(100 * attempt as u64))
                        .await;
                }
                continue;
            }
            Err(error) => {
                last_error = error;
                if attempt < attempts {
                    tokio::time::sleep(std::time::Duration::from_millis(100 * attempt as u64))
                        .await;
                }
                continue;
            }
        };
        let entry = ProviderModelsEntry {
            models: models.clone(),
            checked_at: now,
            last_modified,
            etag,
        };
        match policy {
            RemoteCatalogRefreshPolicy::Cached => store.write(spec.id, entry)?,
            RemoteCatalogRefreshPolicy::Authoritative { .. } => {
                store.write_authoritative(spec.id, entry)?
            }
        }
        return Ok(models);
    }

    Err(format!(
        "remote model catalog for {} failed after {attempts} attempts: {last_error}",
        spec.id
    ))
}

fn sanitized_response_preview(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(160)
        .collect()
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
        assert_eq!(
            provider_spec("sensenova").unwrap().base_url,
            "https://token.sensenova.cn/v1"
        );
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

    #[tokio::test]
    async fn authoritative_remote_catalog_retries_an_empty_response() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = [0u8; 8192];
                let count = socket.read(&mut request).unwrap();
                assert!(
                    String::from_utf8_lossy(&request[..count])
                        .starts_with("GET /api/models/providers/minimax-cn")
                );
                let body = if attempt == 0 {
                    ""
                } else {
                    r#"{"MiniMax-M3":{"id":"MiniMax-M3","api":"anthropic-messages"}}"#
                };
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let home = tempfile::tempdir().unwrap();
        let models = refresh_remote_provider_catalog_with_policy(
            provider_spec("minimax-cn").unwrap(),
            &ProviderModelsStore::open(home.path()),
            &format!("http://{address}"),
            false,
            RemoteCatalogRefreshPolicy::Authoritative { attempts: 2 },
        )
        .await
        .unwrap();
        server.join().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "MiniMax-M3");
    }

    #[tokio::test]
    async fn authoritative_remote_catalog_stops_after_repeated_invalid_json() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = [0u8; 8192];
                socket.read(&mut request).unwrap();
                let body = "<html>temporary edge failure</html>";
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let home = tempfile::tempdir().unwrap();
        let error = refresh_remote_provider_catalog_with_policy(
            provider_spec("minimax-cn").unwrap(),
            &ProviderModelsStore::open(home.path()),
            &format!("http://{address}"),
            false,
            RemoteCatalogRefreshPolicy::Authoritative { attempts: 2 },
        )
        .await
        .unwrap_err();
        server.join().unwrap();
        assert!(error.contains("minimax-cn"), "{error}");
        assert!(error.contains("invalid JSON"), "{error}");
        assert!(error.contains("after 2 attempts"), "{error}");
        assert!(!error.contains("sentinel-api-key"), "{error}");
    }

    #[tokio::test]
    async fn authoritative_remote_catalog_recovers_malformed_store() {
        use std::io::{Read, Write};
        let home = tempfile::tempdir().unwrap();
        let corrupt_bytes = vec![0_u8; 4707];
        std::fs::write(home.path().join("models-store.json"), &corrupt_bytes).unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 8192];
            let count = socket.read(&mut request).unwrap();
            assert!(
                String::from_utf8_lossy(&request[..count])
                    .starts_with("GET /api/models/providers/minimax-cn")
            );
            let body = r#"{"MiniMax-M3":{"id":"MiniMax-M3","api":"anthropic-messages"}}"#;
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let models = refresh_remote_provider_catalog_with_policy(
            provider_spec("minimax-cn").unwrap(),
            &ProviderModelsStore::open(home.path()),
            &format!("http://{address}"),
            false,
            RemoteCatalogRefreshPolicy::Authoritative { attempts: 1 },
        )
        .await
        .unwrap();
        server.join().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "MiniMax-M3");

        let replacement = std::fs::read(home.path().join("models-store.json")).unwrap();
        serde_json::from_slice::<serde_json::Value>(&replacement).unwrap();
        let backups = std::fs::read_dir(home.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("models-store.json.corrupt-")
            })
            .collect::<Vec<_>>();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(backups[0].path()).unwrap(), corrupt_bytes);
    }
}
