use crate::{
    RuntimePromptOutcome, RuntimeSession, ToolApproval, TranscriptStore, import_legacy_if_needed,
};
use lato_ai::{
    CATALOG, CredentialStore, CustomHttpModelStream, CustomModel, FakeModelStream, HttpModelStream,
    ModelStream, StreamPiece, SwitchableModelStream, adapt_model_stream, api_key_login_allowed,
    custom_model_auth, dialect_implemented, get_auth_refreshing, load_models_json, lookup_model,
    oauth_allowed, phase0_supported, store_oauth,
};
use lato_core::{EventStore, JournalReplay, SessionId};
use lato_mcp::{PluginOrigin, PluginPackage, discover_plugin};
use lato_protocol::{JsonRpcReq, METHODS_IMPLEMENTED, PROTOCOL_VERSION, err, is_implemented, ok};
use lato_store::{FileEventStore, derive_automatic_title};
use lato_workspace::{ApprovalMode, FileLocks, SessionTrust};
use std::{collections::HashMap, path::PathBuf, sync::Arc};

pub struct AcpHost {
    sessions: HashMap<String, Arc<RuntimeSession>>,
    pub updates: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    next_id: usize,
    cwd: PathBuf,
    trust: SessionTrust,
    stream: Arc<SwitchableModelStream>,
    locks: Arc<FileLocks>,
    pub prompts_via_acp: usize,
    model: (String, String),
    transcripts: Option<TranscriptStore>,
    events: Option<Arc<FileEventStore>>,
    credentials: Option<CredentialStore>,
    custom_models: Vec<CustomModel>,
    plugins: Vec<PluginPackage>,
    tool_approval: Option<Arc<dyn ToolApproval>>,
}

impl AcpHost {
    pub fn new(
        cwd: PathBuf,
        trust: SessionTrust,
        updates: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
        stream: Arc<dyn ModelStream>,
    ) -> Self {
        Self::new_with_approval(cwd, trust, updates, stream, None)
    }

    pub fn new_with_approval(
        cwd: PathBuf,
        trust: SessionTrust,
        updates: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
        stream: Arc<dyn ModelStream>,
        tool_approval: Option<Arc<dyn ToolApproval>>,
    ) -> Self {
        let stream = Arc::new(SwitchableModelStream::new(stream));
        let lato_home = std::env::var_os("LATO_HOME").map(PathBuf::from);
        let transcripts = lato_home
            .as_deref()
            .and_then(|home| TranscriptStore::open(home).ok());
        let events = lato_home
            .as_deref()
            .and_then(|home| FileEventStore::open(home).ok())
            .map(Arc::new);
        let credentials = lato_home
            .as_deref()
            .and_then(|home| CredentialStore::open(home).ok());
        let custom_models = lato_home
            .as_deref()
            .and_then(|home| load_models_json(&home.join("models.json")).ok())
            .unwrap_or_default();
        let plugins = discover_plugins(&cwd, lato_home.as_deref(), trust.cwd_trusted());
        Self {
            sessions: HashMap::new(),
            updates,
            next_id: 1,
            cwd,
            trust,
            stream,
            locks: Arc::new(FileLocks::new()),
            prompts_via_acp: 0,
            model: ("openai".into(), "gpt-4.1".into()),
            transcripts,
            events,
            credentials,
            custom_models,
            plugins,
            tool_approval,
        }
    }

    async fn make_runtime_session(
        &self,
        sid: &str,
        replay: Option<JournalReplay>,
    ) -> Result<Arc<RuntimeSession>, String> {
        if let Some(events) = &self.events {
            let replay = match replay {
                Some(replay) => replay,
                None => events
                    .replay(&SessionId::from(sid))
                    .await
                    .map_err(|error| error.to_string())?,
            };
            let store: Arc<dyn EventStore> = events.clone();
            return RuntimeSession::new_with_store(
                sid.to_string(),
                self.stream.clone(),
                self.locks.clone(),
                self.trust.clone(),
                self.cwd.clone(),
                self.updates.clone(),
                self.tool_approval.clone(),
                store,
                replay,
            )
            .await
            .map(Arc::new)
            .map_err(|error| error.to_string());
        }
        Ok(Arc::new(RuntimeSession::new(
            sid.to_string(),
            self.stream.clone(),
            self.locks.clone(),
            self.trust.clone(),
            self.cwd.clone(),
            self.updates.clone(),
            self.tool_approval.clone(),
        )))
    }

    async fn session_exists(&self, sid: &str) -> Result<bool, String> {
        if self.sessions.contains_key(sid) {
            return Ok(true);
        }
        if let Some(store) = &self.transcripts
            && store
                .list()
                .map_err(|error| error.to_string())?
                .iter()
                .any(|id| id == sid)
        {
            return Ok(true);
        }
        if let Some(store) = &self.events
            && store
                .list_sessions()
                .await
                .map_err(|error| error.to_string())?
                .iter()
                .any(|id| id.as_str() == sid)
        {
            return Ok(true);
        }
        Ok(false)
    }

    pub async fn handle(&mut self, req: JsonRpcReq) -> Option<serde_json::Value> {
        let id = req.id.clone();
        if !is_implemented(&req.method) {
            return Some(err(id, -32601, "method not found"));
        }
        match req.method.as_str() {
            "initialize" => Some(ok(
                id,
                serde_json::json!({"protocolVersion": PROTOCOL_VERSION, "agentCapabilities": {"methods": METHODS_IMPLEMENTED}}),
            )),
            "session/new" => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let sid = format!("s{now}-{}", self.next_id);
                self.next_id += 1;
                let session = match self.make_runtime_session(&sid, None).await {
                    Ok(session) => session,
                    Err(error) => return Some(err(id, -32000, error)),
                };
                self.sessions.insert(sid.clone(), session);
                Some(ok(id, serde_json::json!({"sessionId": sid})))
            }
            "session/prompt" => {
                self.prompts_via_acp += 1;
                let p = req.params.unwrap_or_default();
                let sid = p.get("sessionId").and_then(|v| v.as_str()).unwrap_or("s1");
                let text = p
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let Some(session) = self.sessions.get(sid).cloned() else {
                    return Some(err(id, -32000, "unknown session"));
                };
                if self.trust.mode == ApprovalMode::Ask && text.contains("tool") {
                    let _ = self.updates.send(serde_json::json!({"jsonrpc":"2.0","id":format!("permission-{sid}"),"method":"session/request_permission","params":{"sessionId": sid,"options":["allow_once","allow_session","deny","cancel"]}}));
                }
                let outcome = session.prompt(text.clone()).await;
                if let Some(events) = &self.events {
                    let _ = events
                        .ensure_automatic_title(&SessionId::from(sid), &text)
                        .await;
                }
                match outcome {
                    Ok(RuntimePromptOutcome::Complete { text }) => {
                        let _ = self.updates.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":sid,"text":text}}));
                        Some(ok(id, serde_json::json!({"status":"complete","text":text})))
                    }
                    Ok(RuntimePromptOutcome::Cancelled { .. }) => {
                        Some(ok(id, serde_json::json!({"status":"cancelled","text":""})))
                    }
                    Err(error) => Some(err(id, -32000, error.to_string())),
                }
            }
            "session/cancel" => {
                if let Some(sid) = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|v| v.as_str())
                    && let Some(session) = self.sessions.get(sid).cloned()
                {
                    let _ = session.cancel().await;
                }
                Some(ok(id, serde_json::json!({"status":"cancelled"})))
            }
            "session/list" => {
                let mut sessions: Vec<String> = self.sessions.keys().cloned().collect();
                if let Some(store) = &self.transcripts
                    && let Ok(on_disk) = store.list()
                {
                    sessions.extend(on_disk);
                }
                if let Some(store) = &self.events
                    && let Ok(on_disk) = store.list_sessions().await
                {
                    sessions.extend(on_disk.into_iter().map(|session| session.to_string()));
                }
                sessions.sort();
                sessions.dedup();
                Some(ok(id, serde_json::json!({"sessions": sessions})))
            }
            "lato/session/list" => {
                let mut summaries = std::collections::BTreeMap::<String, serde_json::Value>::new();
                if let Some(store) = &self.events {
                    match store.list_session_summaries().await {
                        Ok(items) => {
                            for item in items {
                                summaries.insert(
                                    item.session_id.to_string(),
                                    serde_json::to_value(item).unwrap_or_default(),
                                );
                            }
                        }
                        Err(error) => return Some(err(id, -32000, error.to_string())),
                    }
                }
                if let Some(store) = &self.transcripts {
                    let legacy = match store.list() {
                        Ok(items) => items,
                        Err(error) => return Some(err(id, -32000, error)),
                    };
                    for sid in legacy {
                        if summaries.contains_key(&sid) {
                            continue;
                        }
                        let title = store
                            .load_optional(&sid)
                            .ok()
                            .flatten()
                            .and_then(|history| {
                                history.into_iter().find_map(|item| match item {
                                    crate::HistoryItem::User(text) => Some(text),
                                    _ => None,
                                })
                            })
                            .map(|text| derive_automatic_title(&text))
                            .unwrap_or_else(|| "New session".into());
                        summaries.insert(
                            sid.clone(),
                            serde_json::json!({
                                "sessionId": sid,
                                "title": title,
                                "titleSource": "automatic",
                                "createdAtMs": 0,
                                "updatedAtMs": 0,
                            }),
                        );
                    }
                }
                for sid in self.sessions.keys() {
                    summaries.entry(sid.clone()).or_insert_with(|| {
                        serde_json::json!({
                            "sessionId": sid,
                            "title": "New session",
                            "titleSource": "automatic",
                            "createdAtMs": 0,
                            "updatedAtMs": 0,
                        })
                    });
                }
                let mut summaries = summaries.into_values().collect::<Vec<_>>();
                summaries.sort_by(|left, right| {
                    right["updatedAtMs"]
                        .as_u64()
                        .cmp(&left["updatedAtMs"].as_u64())
                        .then_with(|| right["sessionId"].as_str().cmp(&left["sessionId"].as_str()))
                });
                Some(ok(id, serde_json::json!({"sessions": summaries})))
            }
            "lato/session/rename" => {
                let params = req.params.unwrap_or_default();
                let Some(sid) = params.get("sessionId").and_then(|value| value.as_str()) else {
                    return Some(err(id, -32602, "sessionId is required"));
                };
                let Some(title) = params.get("title").and_then(|value| value.as_str()) else {
                    return Some(err(id, -32602, "title is required"));
                };
                let session_id = match SessionId::parse(sid) {
                    Ok(session_id) => session_id,
                    Err(error) => return Some(err(id, -32602, error.to_string())),
                };
                match self.session_exists(sid).await {
                    Ok(true) => {}
                    Ok(false) => return Some(err(id, -32000, "unknown session")),
                    Err(error) => return Some(err(id, -32000, error)),
                }
                if let Some(session) = self.sessions.get(sid)
                    && session.is_active().await
                {
                    return Some(err(id, -32000, "session_busy"));
                }
                let Some(store) = &self.events else {
                    return Some(err(id, -32000, "session metadata unavailable"));
                };
                if let Err(error) =
                    import_legacy_if_needed(&session_id, self.transcripts.as_ref(), store.as_ref())
                        .await
                {
                    return Some(err(id, -32000, error.to_string()));
                }
                match store.rename_session(&session_id, title).await {
                    Ok(summary) => Some(ok(id, serde_json::to_value(summary).unwrap_or_default())),
                    Err(error) => Some(err(id, -32000, error.to_string())),
                }
            }
            "lato/session/delete" => {
                let params = req.params.unwrap_or_default();
                let Some(sid) = params.get("sessionId").and_then(|value| value.as_str()) else {
                    return Some(err(id, -32602, "sessionId is required"));
                };
                let session_id = match SessionId::parse(sid) {
                    Ok(session_id) => session_id,
                    Err(error) => return Some(err(id, -32602, error.to_string())),
                };
                match self.session_exists(sid).await {
                    Ok(true) => {}
                    Ok(false) => return Some(err(id, -32000, "unknown session")),
                    Err(error) => return Some(err(id, -32000, error)),
                }
                if let Some(session) = self.sessions.get(sid)
                    && session.is_active().await
                {
                    return Some(err(id, -32000, "session_busy"));
                }
                if let Some(session) = self.sessions.remove(sid)
                    && let Err(error) = session.shutdown().await
                {
                    return Some(err(id, -32000, error.to_string()));
                }
                if let Some(store) = &self.events
                    && let Err(error) = store.delete_session(&session_id).await
                {
                    return Some(err(id, -32000, error.to_string()));
                }
                if let Some(store) = &self.transcripts
                    && let Err(error) = store.delete(sid)
                {
                    return Some(err(id, -32000, error));
                }
                Some(ok(id, serde_json::json!({"deleted": true})))
            }
            "session/close" => {
                if let Some(sid) = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|v| v.as_str())
                    && let Some(session) = self.sessions.remove(sid)
                {
                    let _ = session.shutdown().await;
                }
                Some(ok(id, serde_json::json!({"closed": true})))
            }
            "session/resume" => {
                let sid = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("s1");
                match self.session_exists(sid).await {
                    Ok(true) => {}
                    Ok(false) => return Some(err(id, -32000, "unknown session")),
                    Err(error) => return Some(err(id, -32000, error)),
                }
                if !self.sessions.contains_key(sid) {
                    let replay = if let Some(events) = &self.events {
                        match import_legacy_if_needed(
                            &SessionId::from(sid),
                            self.transcripts.as_ref(),
                            events.as_ref(),
                        )
                        .await
                        {
                            Ok(replay) => Some(replay),
                            Err(error) => {
                                return Some(err(id, -32000, error.to_string()));
                            }
                        }
                    } else {
                        None
                    };
                    let session = match self.make_runtime_session(sid, replay).await {
                        Ok(session) => session,
                        Err(error) => return Some(err(id, -32000, error)),
                    };
                    if self.events.is_none()
                        && let Some(store) = &self.transcripts
                        && let Ok(Some(history)) = store.load_optional(sid)
                    {
                        session.replace_history(history).await;
                    }
                    self.sessions.insert(sid.into(), session);
                }
                Some(ok(
                    id,
                    serde_json::json!({"sessionId": sid, "replayed": false}),
                ))
            }
            "session/set_model" => {
                let p = req.params.unwrap_or_default();
                let provider = p
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .unwrap_or("openai");
                let model = p
                    .get("model")
                    .or_else(|| p.get("modelId"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("gpt-4.1");
                let supported = lookup_model(provider, model)
                    .map(|model| phase0_supported(model.api))
                    .or_else(|| {
                        self.custom_models
                            .iter()
                            .find(|entry| entry.provider == provider && entry.id == model)
                            .map(|entry| dialect_implemented(entry.api))
                    });
                match supported {
                    Some(true) => {}
                    Some(false) => return Some(err(id, -32000, "dialect_unimplemented")),
                    None => return Some(err(id, -32000, "unknown model")),
                }
                if let Some(catalog_model) = lookup_model(provider, model) {
                    if let Some(store) = self.credentials.as_mut() {
                        match get_auth_refreshing(
                            store,
                            provider,
                            &|name| std::env::var(name).ok(),
                            None,
                            &reqwest::Client::new(),
                        )
                        .await
                        {
                            Ok(Some(auth)) => {
                                let raw: Arc<dyn ModelStream> =
                                    Arc::new(HttpModelStream::new(catalog_model, auth));
                                let adapted = match adapt_model_stream(provider, model, raw) {
                                    Ok(adapted) => adapted,
                                    Err(error) => {
                                        return Some(err(
                                            id,
                                            -32000,
                                            format!("invalid model selection: {error}"),
                                        ));
                                    }
                                };
                                self.stream.set(adapted).await
                            }
                            Ok(None) => {}
                            Err(error) => {
                                return Some(err(
                                    id,
                                    -32000,
                                    format!("oauth refresh failed: {error}"),
                                ));
                            }
                        }
                    }
                } else if let Some(custom) = self
                    .custom_models
                    .iter()
                    .find(|entry| entry.provider == provider && entry.id == model)
                    .cloned()
                    && let Some(auth) = custom_model_auth(&custom, &|name| std::env::var(name).ok())
                {
                    let raw: Arc<dyn ModelStream> =
                        Arc::new(CustomHttpModelStream::new(custom, auth));
                    let adapted = match adapt_model_stream(provider, model, raw) {
                        Ok(adapted) => adapted,
                        Err(error) => {
                            return Some(err(
                                id,
                                -32000,
                                format!("invalid model selection: {error}"),
                            ));
                        }
                    };
                    self.stream.set(adapted).await;
                }
                self.model = (provider.into(), model.into());
                Some(ok(id, serde_json::json!({"supported": true})))
            }
            "lato/models/list" => {
                let mut models = CATALOG.iter().map(|m| serde_json::json!({"provider":m.provider,"id":m.id,"supported":phase0_supported(m.api),"reason": if phase0_supported(m.api) { serde_json::Value::Null } else { serde_json::json!("dialect_unimplemented") }})).collect::<Vec<_>>();
                models.extend(self.custom_models.iter().map(|m| serde_json::json!({
                    "provider":m.provider,"id":m.id,"api":m.api,"baseUrl":m.base_url,
                    "supported":dialect_implemented(m.api),
                    "reason":if dialect_implemented(m.api) { serde_json::Value::Null } else { serde_json::json!("dialect_unimplemented") }
                })));
                Some(ok(id, serde_json::json!({"models":models})))
            }
            "lato/session/info" => Some(ok(id, serde_json::json!({"cwd": self.cwd}))),
            "lato/auth/login" => {
                let p = req.params.unwrap_or_default();
                let provider = p.get("provider").and_then(|v| v.as_str()).unwrap_or("");
                let method = p
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("api_key");
                let Some(store) = self.credentials.as_mut() else {
                    return Some(err(id, -32000, "LATO_HOME credential store unavailable"));
                };
                match method {
                    "api_key" => {
                        if !api_key_login_allowed(provider) {
                            return Some(err(
                                id,
                                -32000,
                                "api key login not supported for provider",
                            ));
                        }
                        let Some(key) = p.get("key").and_then(|v| v.as_str()) else {
                            return Some(err(id, -32602, "missing secret key"));
                        };
                        if let Err(e) = store.modify(|entries| {
                            entries.insert(
                                provider.into(),
                                serde_json::json!({"type":"api_key","key":key}),
                            );
                        }) {
                            return Some(err(id, -32000, e.to_string()));
                        }
                    }
                    "oauth" => {
                        if !oauth_allowed(provider) {
                            return Some(err(id, -32000, "oauth not supported for provider"));
                        }
                        let access = p.get("access").and_then(|v| v.as_str()).unwrap_or("");
                        let refresh = p.get("refresh").and_then(|v| v.as_str()).unwrap_or("");
                        let expires = p.get("expires").and_then(|v| v.as_i64()).unwrap_or(0);
                        let account_id = p
                            .get("account_id")
                            .or_else(|| p.get("accountId"))
                            .and_then(|v| v.as_str());
                        if access.is_empty() || refresh.is_empty() {
                            return Some(err(id, -32602, "oauth interaction required"));
                        }
                        if let Err(e) =
                            store_oauth(store, provider, access, refresh, expires, account_id)
                        {
                            return Some(err(id, -32000, e.to_string()));
                        }
                    }
                    _ => return Some(err(id, -32602, "unknown login method")),
                }
                Some(ok(
                    id,
                    serde_json::json!({"ok": true,"provider":provider,"method":method}),
                ))
            }
            "lato/auth/logout" => {
                let provider = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("provider"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let Some(store) = self.credentials.as_mut() else {
                    return Some(err(id, -32000, "LATO_HOME credential store unavailable"));
                };
                match store.modify(|entries| {
                    entries.remove(provider);
                }) {
                    Ok(()) => Some(ok(id, serde_json::json!({"ok":true}))),
                    Err(e) => Some(err(id, -32000, e.to_string())),
                }
            }
            "lato/plugins/reload" => {
                let lato_home = std::env::var_os("LATO_HOME").map(PathBuf::from);
                self.plugins =
                    discover_plugins(&self.cwd, lato_home.as_deref(), self.trust.cwd_trusted());
                Some(ok(
                    id,
                    serde_json::json!({
                        "plugins":self.plugins.iter().map(|plugin| serde_json::json!({
                            "root":plugin.root,"trusted":plugin.trusted,"hooksEnabled":plugin.hooks_enabled,
                            "mcpEnabled":plugin.mcp_enabled,"skills":plugin.skills
                        })).collect::<Vec<_>>()
                    }),
                ))
            }
            "lato/auth/status" => {
                let provider = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("provider"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let credential_type = self
                    .credentials
                    .as_ref()
                    .and_then(|store| store.get(provider))
                    .map(|credential| match credential {
                        lato_ai::Credential::ApiKey { .. } => "api_key",
                        lato_ai::Credential::Oauth { .. } => "oauth",
                    });
                Some(ok(
                    id,
                    serde_json::json!({"provider":provider,"configured":credential_type.is_some(),"type":credential_type}),
                ))
            }
            _ => Some(ok(id, serde_json::json!({"ok": true}))),
        }
    }
}

fn discover_plugins(
    cwd: &std::path::Path,
    lato_home: Option<&std::path::Path>,
    project_trusted: bool,
) -> Vec<PluginPackage> {
    let mut plugins = Vec::new();
    let locations = [
        (cwd.join(".lato/plugins"), PluginOrigin::Project),
        (
            lato_home
                .map(|home| home.join("plugins"))
                .unwrap_or_default(),
            PluginOrigin::User,
        ),
    ];
    for (location, origin) in locations {
        let Ok(entries) = std::fs::read_dir(location) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Ok(plugin) = discover_plugin(&entry.path(), origin, project_trusted) {
                plugins.push(plugin);
            }
        }
    }
    plugins.sort_by(|a, b| a.root.cmp(&b.root));
    plugins
}

pub fn default_fake_stream() -> Arc<dyn ModelStream> {
    Arc::new(FakeModelStream::new(vec![vec![StreamPiece::Text(
        "hi".into(),
    )]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_model_constructors_cross_the_canonical_model_port_boundary() {
        let source = include_str!("host.rs");
        let boundary_call = ["adapt_model_stream", "(provider, model"].concat();
        assert_eq!(source.matches(&boundary_call).count(), 2);
    }

    fn req(id: i32, method: &str, params: serde_json::Value) -> JsonRpcReq {
        JsonRpcReq {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(id)),
            method: method.into(),
            params: Some(params),
        }
    }
    fn host() -> AcpHost {
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let cwd = std::env::current_dir().unwrap();
        AcpHost::new(
            cwd.clone(),
            SessionTrust::for_headless_prompt(cwd),
            tx,
            default_fake_stream(),
        )
    }

    #[tokio::test]
    async fn a1_2_prompt_text() {
        let mut h = host();
        h.handle(req(1, "initialize", serde_json::json!({})))
            .await
            .unwrap();
        let r = h
            .handle(req(2, "session/new", serde_json::json!({})))
            .await
            .unwrap();
        let sid = r["result"]["sessionId"].as_str().unwrap().to_string();
        let p = h
            .handle(req(
                3,
                "session/prompt",
                serde_json::json!({"sessionId":sid,"text":"hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(p["result"]["status"], "complete");
    }
    #[tokio::test]
    async fn a1_3_cancel_then_reprompt() {
        let mut h = host();
        let r = h
            .handle(req(1, "session/new", serde_json::json!({})))
            .await
            .unwrap();
        let sid = r["result"]["sessionId"].as_str().unwrap().to_string();
        h.handle(req(
            2,
            "session/cancel",
            serde_json::json!({"sessionId":sid}),
        ))
        .await
        .unwrap();
        let p = h
            .handle(req(
                3,
                "session/prompt",
                serde_json::json!({"sessionId":sid,"text":"again"}),
            ))
            .await
            .unwrap();
        assert!(p.get("result").is_some());
    }
    #[tokio::test]
    async fn a1_4_request_permission_for_ask() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cwd = std::env::current_dir().unwrap();
        let mut h = AcpHost::new(
            cwd.clone(),
            SessionTrust::for_interactive(cwd, true),
            tx,
            default_fake_stream(),
        );
        let r = h
            .handle(req(1, "session/new", serde_json::json!({})))
            .await
            .unwrap();
        let sid = r["result"]["sessionId"].as_str().unwrap().to_string();
        h.handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId":sid,"text":"please tool"}),
        ))
        .await
        .unwrap();
        let n = rx.try_recv().unwrap();
        assert_eq!(n["method"], "session/request_permission");
    }
    #[tokio::test]
    async fn a1_5_list_close_resume() {
        let directory = tempfile::tempdir().unwrap();
        let mut h = host();
        h.events = Some(Arc::new(FileEventStore::open(directory.path()).unwrap()));
        let r = h
            .handle(req(1, "session/new", serde_json::json!({})))
            .await
            .unwrap();
        let sid = r["result"]["sessionId"].as_str().unwrap().to_string();
        assert!(
            h.handle(req(2, "session/list", serde_json::json!({})))
                .await
                .unwrap()["result"]["sessions"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(sid.clone()))
        );
        h.handle(req(
            3,
            "session/close",
            serde_json::json!({"sessionId":sid}),
        ))
        .await
        .unwrap();
        let rr = h
            .handle(req(
                4,
                "session/resume",
                serde_json::json!({"sessionId":sid}),
            ))
            .await
            .unwrap();
        assert_eq!(rr["result"]["replayed"], false);
    }

    #[tokio::test]
    async fn resume_rejects_unknown_session_without_creating_it() {
        let directory = tempfile::tempdir().unwrap();
        let mut h = host();
        h.events = Some(Arc::new(FileEventStore::open(directory.path()).unwrap()));
        let response = h
            .handle(req(
                1,
                "session/resume",
                serde_json::json!({"sessionId": "s1700000000000-404"}),
            ))
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], -32000);
        assert_eq!(response["error"]["message"], "unknown session");

        let listed = h
            .handle(req(2, "session/list", serde_json::json!({})))
            .await
            .unwrap();
        assert!(
            !listed["result"]["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == "s1700000000000-404")
        );
    }
    #[tokio::test]
    async fn a1_6_set_model_rejects_unsupported_catalog_model() {
        let mut h = host();
        let e = h
            .handle(req(
                1,
                "session/set_model",
                serde_json::json!({"provider":"radius","model":"radius-test"}),
            ))
            .await
            .unwrap();
        assert!(e.get("error").is_some());
        let ok = h
            .handle(req(
                2,
                "session/set_model",
                serde_json::json!({"provider":"openai","model":"gpt-4.1"}),
            ))
            .await
            .unwrap();
        assert!(ok.get("result").is_some());
    }
    #[tokio::test]
    async fn e5_1_plugins_reload_preserves_project_trust_boundary() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".lato/plugins/demo/hooks")).unwrap();
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let mut h = AcpHost::new(
            d.path().to_path_buf(),
            SessionTrust::for_interactive(d.path(), false),
            tx,
            default_fake_stream(),
        );
        let response = h
            .handle(req(1, "lato/plugins/reload", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(response["result"]["plugins"][0]["trusted"], false);
        assert_eq!(response["result"]["plugins"][0]["hooksEnabled"], false);
    }

    #[tokio::test]
    async fn a4_6_acp_auth_status_uses_shared_store_without_secret() {
        let d = tempfile::tempdir().unwrap();
        let mut h = host();
        h.credentials = Some(CredentialStore::open(d.path()).unwrap());
        let login = h
            .handle(req(
                1,
                "lato/auth/login",
                serde_json::json!({"provider":"openai","method":"api_key","key":"sk-secret"}),
            ))
            .await
            .unwrap();
        assert_eq!(login["result"]["ok"], true);
        let status = h
            .handle(req(
                2,
                "lato/auth/status",
                serde_json::json!({"provider":"openai"}),
            ))
            .await
            .unwrap();
        assert_eq!(status["result"]["configured"], true);
        assert_eq!(status["result"]["type"], "api_key");
        assert!(!status.to_string().contains("sk-secret"));
        h.handle(req(
            3,
            "lato/auth/logout",
            serde_json::json!({"provider":"openai"}),
        ))
        .await
        .unwrap();
        assert!(h.credentials.as_ref().unwrap().get("openai").is_none());
    }

    #[tokio::test]
    async fn a1_8_load_errors() {
        let mut h = host();
        let e = h
            .handle(req(1, "session/load", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(e["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn runtime_resume_hydrates_transcript_history() {
        let directory = tempfile::tempdir().unwrap();
        let store = TranscriptStore::open(directory.path()).unwrap();
        store
            .append(
                "resume-1",
                &[
                    crate::HistoryItem::User("old".into()),
                    crate::HistoryItem::AssistantText("answer".into()),
                ],
            )
            .unwrap();
        let mut host = host();
        host.transcripts = Some(store);
        host.handle(req(
            1,
            "session/resume",
            serde_json::json!({"sessionId": "resume-1"}),
        ))
        .await
        .unwrap();
        let history = host.sessions["resume-1"].history_snapshot().await;
        assert_eq!(history.len(), 2);
    }

    #[tokio::test]
    async fn resumed_transcript_is_preserved_while_new_rows_go_to_the_journal() {
        let directory = tempfile::tempdir().unwrap();
        let store = TranscriptStore::open(directory.path()).unwrap();
        store
            .append(
                "resume-append",
                &[
                    crate::HistoryItem::User("old".into()),
                    crate::HistoryItem::AssistantText("answer".into()),
                ],
            )
            .unwrap();
        let mut host = host();
        host.transcripts = Some(store.clone());
        host.events = Some(Arc::new(FileEventStore::open(directory.path()).unwrap()));
        host.handle(req(
            1,
            "session/resume",
            serde_json::json!({"sessionId": "resume-append"}),
        ))
        .await
        .unwrap();
        host.handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId": "resume-append", "text": "new"}),
        ))
        .await
        .unwrap();
        let loaded = store.load("resume-append").unwrap();
        let old_user_rows = loaded
            .iter()
            .filter(|item| matches!(item, crate::HistoryItem::User(text) if text == "old"))
            .count();
        assert_eq!(old_user_rows, 1);
        assert_eq!(loaded.len(), 2);
        let replay = host
            .events
            .as_ref()
            .unwrap()
            .replay(&SessionId::from("resume-append"))
            .await
            .unwrap();
        assert!(replay.projection.messages.len() > 2);
    }

    #[tokio::test]
    async fn session_list_unions_legacy_and_journal_ids_without_duplicates() {
        let directory = tempfile::tempdir().unwrap();
        let transcripts = TranscriptStore::open(directory.path()).unwrap();
        transcripts
            .append("shared", &[crate::HistoryItem::User("legacy".into())])
            .unwrap();
        transcripts
            .append("legacy-only", &[crate::HistoryItem::User("legacy".into())])
            .unwrap();
        let events = Arc::new(FileEventStore::open(directory.path()).unwrap());
        for sid in ["shared", "journal-only"] {
            events
                .append(
                    lato_core::JournalEnvelope {
                        schema_version: lato_core::JOURNAL_SCHEMA_VERSION,
                        record_id: lato_core::JournalRecordId::from(format!("{sid}-0")),
                        session_id: SessionId::from(sid),
                        turn_id: None,
                        journal_sequence: 0,
                        timestamp_ms: 0,
                        record: lato_core::JournalRecord::SessionStarted,
                    },
                    lato_core::JournalDurability::SyncData,
                )
                .await
                .unwrap();
        }
        let mut host = host();
        host.transcripts = Some(transcripts);
        host.events = Some(events);
        let response = host
            .handle(req(1, "session/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(
            response["result"]["sessions"],
            serde_json::json!(["journal-only", "legacy-only", "shared"])
        );
    }

    #[tokio::test]
    async fn session_admin_extensions_preserve_legacy_list_shape() {
        let directory = tempfile::tempdir().unwrap();
        let mut host = host();
        host.events = Some(Arc::new(FileEventStore::open(directory.path()).unwrap()));
        let created = host
            .handle(req(1, "session/new", serde_json::json!({})))
            .await
            .unwrap();
        let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
        host.handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "Implement session titles"}),
        ))
        .await
        .unwrap();

        let legacy = host
            .handle(req(3, "session/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(legacy["result"]["sessions"], serde_json::json!([sid]));
        let listed = host
            .handle(req(4, "lato/session/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(
            listed["result"]["sessions"][0]["title"],
            "Implement session titles"
        );
        assert_eq!(listed["result"]["sessions"][0]["titleSource"], "automatic");

        let renamed = host
            .handle(req(
                5,
                "lato/session/rename",
                serde_json::json!({"sessionId": sid, "title": "Manual title"}),
            ))
            .await
            .unwrap();
        assert_eq!(renamed["result"]["title"], "Manual title");
        assert_eq!(renamed["result"]["titleSource"], "manual");

        let deleted = host
            .handle(req(
                6,
                "lato/session/delete",
                serde_json::json!({"sessionId": sid}),
            ))
            .await
            .unwrap();
        assert_eq!(deleted["result"]["deleted"], true);
        let listed = host
            .handle(req(7, "session/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(listed["result"]["sessions"], serde_json::json!([]));
    }
}
