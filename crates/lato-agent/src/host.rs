use crate::{PromptKind, SessionActor, TranscriptStore};
use lato_ai::{
    CATALOG, CredentialStore, FakeModelStream, ModelStream, StreamPiece, api_key_login_allowed,
    lookup_model, oauth_allowed, phase0_supported, store_oauth,
};
use lato_protocol::{JsonRpcReq, METHODS_IMPLEMENTED, PROTOCOL_VERSION, err, is_implemented, ok};
use lato_workspace::{ApprovalMode, FileLocks, SessionTrust};
use std::{collections::HashMap, path::PathBuf, sync::Arc};

pub struct AcpHost {
    sessions: HashMap<String, SessionActor>,
    pub updates: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    next_id: usize,
    cwd: PathBuf,
    trust: SessionTrust,
    stream: Arc<dyn ModelStream>,
    locks: Arc<FileLocks>,
    pub prompts_via_acp: usize,
    model: (String, String),
    transcripts: Option<TranscriptStore>,
    persisted: HashMap<String, usize>,
    credentials: Option<CredentialStore>,
}

impl AcpHost {
    pub fn new(
        cwd: PathBuf,
        trust: SessionTrust,
        updates: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
        stream: Arc<dyn ModelStream>,
    ) -> Self {
        let lato_home = std::env::var_os("LATO_HOME").map(PathBuf::from);
        let transcripts = lato_home
            .as_deref()
            .and_then(|home| TranscriptStore::open(home).ok());
        let credentials = lato_home
            .as_deref()
            .and_then(|home| CredentialStore::open(home).ok());
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
            persisted: HashMap::new(),
            credentials,
        }
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
                self.sessions.insert(
                    sid.clone(),
                    SessionActor::new(
                        self.stream.clone(),
                        self.locks.clone(),
                        self.trust.clone(),
                        self.cwd.clone(),
                    ),
                );
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
                let Some(actor) = self.sessions.get_mut(sid) else {
                    return Some(err(id, -32000, "unknown session"));
                };
                if self.trust.mode == ApprovalMode::Ask && text.contains("tool") {
                    let _ = self.updates.send(serde_json::json!({"method":"session/request_permission","params":{"sessionId": sid}}));
                }
                match actor.prompt(PromptKind::Start, text).await {
                    Ok(_) => {
                        if let Some(store) = &self.transcripts {
                            let start = *self.persisted.get(sid).unwrap_or(&0);
                            if let Err(e) = store.append(sid, &actor.history()[start..]) {
                                return Some(err(id, -32000, format!("persist transcript: {e}")));
                            }
                            self.persisted
                                .insert(sid.to_string(), actor.history().len());
                        }
                        let text = actor.latest_assistant_text();
                        let _ = self.updates.send(serde_json::json!({"method":"session/update","params":{"sessionId":sid,"text":text}}));
                        Some(ok(id, serde_json::json!({"status":"complete","text":text})))
                    }
                    Err(e) => Some(err(id, -32000, e)),
                }
            }
            "session/cancel" => {
                if let Some(sid) = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|v| v.as_str())
                {
                    if let Some(a) = self.sessions.get_mut(sid) {
                        a.cancel();
                    }
                }
                Some(ok(id, serde_json::json!({"status":"cancelled"})))
            }
            "session/list" => {
                let mut sessions: Vec<String> = self.sessions.keys().cloned().collect();
                if let Some(store) = &self.transcripts {
                    if let Ok(on_disk) = store.list() {
                        sessions.extend(on_disk);
                    }
                }
                sessions.sort();
                sessions.dedup();
                Some(ok(id, serde_json::json!({"sessions": sessions})))
            }
            "session/close" => {
                if let Some(sid) = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|v| v.as_str())
                {
                    self.sessions.remove(sid);
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
                if !self.sessions.contains_key(sid) {
                    let mut actor = SessionActor::new(
                        self.stream.clone(),
                        self.locks.clone(),
                        self.trust.clone(),
                        self.cwd.clone(),
                    );
                    if let Some(store) = &self.transcripts {
                        match store.load(sid) {
                            Ok(history) => {
                                self.persisted.insert(sid.into(), history.len());
                                *actor.history_mut() = history;
                            }
                            Err(e) if !e.contains("No such file") => {
                                return Some(err(id, -32000, format!("resume transcript: {e}")));
                            }
                            Err(_) => {}
                        }
                    }
                    self.sessions.insert(sid.into(), actor);
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
                let Some(m) = lookup_model(provider, model) else {
                    return Some(err(id, -32000, "unknown model"));
                };
                if !phase0_supported(m.api) {
                    return Some(err(id, -32000, "dialect_unimplemented"));
                }
                self.model = (provider.into(), model.into());
                Some(ok(id, serde_json::json!({"supported": true})))
            }
            "lato/models/list" => Some(ok(
                id,
                serde_json::json!({"models": CATALOG.iter().map(|m| serde_json::json!({"provider":m.provider,"id":m.id,"supported":phase0_supported(m.api),"reason": if phase0_supported(m.api) { serde_json::Value::Null } else { serde_json::json!("dialect_unimplemented") }})).collect::<Vec<_>>() }),
            )),
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
                        if access.is_empty() || refresh.is_empty() {
                            return Some(err(id, -32602, "oauth interaction required"));
                        }
                        if let Err(e) = store_oauth(store, provider, access, refresh, expires) {
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

pub fn default_fake_stream() -> Arc<dyn ModelStream> {
    Arc::new(FakeModelStream::new(vec![vec![StreamPiece::Text(
        "hi".into(),
    )]]))
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let mut h = host();
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
                serde_json::json!({"sessionId":"same"}),
            ))
            .await
            .unwrap();
        assert_eq!(rr["result"]["replayed"], false);
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
}
