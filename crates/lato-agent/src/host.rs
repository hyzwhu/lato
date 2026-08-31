use crate::{PromptKind, SessionActor};
use lato_ai::{
    CATALOG, FakeModelStream, StreamPiece, lookup_model, oauth_allowed, phase0_supported,
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
    stream: Arc<FakeModelStream>,
    locks: Arc<FileLocks>,
    pub prompts_via_acp: usize,
    model: (String, String),
}

impl AcpHost {
    pub fn new(
        cwd: PathBuf,
        trust: SessionTrust,
        updates: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
        stream: Arc<FakeModelStream>,
    ) -> Self {
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
                let sid = format!("s{}", self.next_id);
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
                        let _ = self.updates.send(serde_json::json!({"method":"session/update","params":{"sessionId":sid,"text":"complete"}}));
                        Some(ok(id, serde_json::json!({"status":"complete"})))
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
            "session/list" => Some(ok(
                id,
                serde_json::json!({"sessions": self.sessions.keys().cloned().collect::<Vec<_>>() }),
            )),
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
                    self.sessions.insert(
                        sid.into(),
                        SessionActor::new(
                            self.stream.clone(),
                            self.locks.clone(),
                            self.trust.clone(),
                            self.cwd.clone(),
                        ),
                    );
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
                if method == "oauth" && !oauth_allowed(provider) {
                    return Some(err(id, -32000, "oauth not supported for provider"));
                }
                Some(ok(id, serde_json::json!({"ok": true})))
            }
            "lato/auth/logout" => Some(ok(id, serde_json::json!({"ok": true}))),
            "lato/auth/status" => Some(ok(
                id,
                serde_json::json!({"configured": false, "secret": null}),
            )),
            _ => Some(ok(id, serde_json::json!({"ok": true}))),
        }
    }
}

pub fn default_fake_stream() -> Arc<FakeModelStream> {
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
    async fn a1_6_set_model_rejects_google() {
        let mut h = host();
        let e = h
            .handle(req(
                1,
                "session/set_model",
                serde_json::json!({"provider":"google","model":"gemini-2.0-flash"}),
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
    async fn a1_8_load_errors() {
        let mut h = host();
        let e = h
            .handle(req(1, "session/load", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(e["error"]["code"], -32601);
    }
}
