use lato_agent::{AcpHost, ToolApproval, default_fake_stream};
use lato_ai::ModelStream;
use lato_protocol::JsonRpcReq;
use lato_workspace::SessionTrust;
use std::sync::Arc;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub session_id: String,
    pub title: String,
    pub title_source: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSizeResponse {
    pub message_count: u64,
    pub serialized_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResponse {
    pub status: String,
    pub before: Option<CompactionSizeResponse>,
    pub after: Option<CompactionSizeResponse>,
    pub checkpoint_id: Option<String>,
    pub warning: Option<lato_core::AgentError>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSwitchResponse {
    pub supported: bool,
    pub provider: String,
    pub model: String,
    pub compaction_warning: Option<lato_core::AgentError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientUpdate {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStarted {
        id: String,
        name: String,
        arguments: String,
    },
    ToolFinished {
        id: String,
        result: String,
    },
    ToolFailed {
        id: String,
        error: String,
    },
    PermissionRequested,
    CompactionStarted {
        compaction_id: String,
        trigger: String,
    },
    CompactionCompleted {
        before: CompactionSizeResponse,
        after: CompactionSizeResponse,
        checkpoint_id: String,
        warning: Option<lato_core::AgentError>,
    },
    CompactionFailed {
        code: String,
        message: String,
    },
    CompactionCancelled,
    ContextUsage {
        estimated_input_tokens: u64,
        context_window: Option<u64>,
        utilization_percent: Option<u8>,
    },
    RecoverySuppression(String),
    ModelChanged {
        provider: String,
        model: String,
        warning: Option<lato_core::AgentError>,
    },
    Unknown,
}

impl ClientUpdate {
    pub fn from_json(value: &serde_json::Value) -> Self {
        let method = value.get("method").and_then(serde_json::Value::as_str);
        let params = value.get("params").unwrap_or(&serde_json::Value::Null);
        match method {
            Some("session/update") => params
                .get("delta")
                .and_then(serde_json::Value::as_str)
                .map(|text| Self::TextDelta(text.to_string()))
                .unwrap_or(Self::Unknown),
            Some("session/reasoning") => params
                .get("delta")
                .and_then(serde_json::Value::as_str)
                .map(|text| Self::ReasoningDelta(text.to_string()))
                .unwrap_or(Self::Unknown),
            Some("session/tool_call") => {
                let name = params
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let id = params
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(&name)
                    .to_string();
                let arguments = params
                    .get("arguments")
                    .map(serde_json::Value::to_string)
                    .unwrap_or_else(|| "{}".to_string());
                Self::ToolStarted {
                    id,
                    name,
                    arguments,
                }
            }
            Some("session/tool_result") => {
                let id = params
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let output = params
                    .get("result")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if params.get("status").and_then(serde_json::Value::as_str) == Some("error") {
                    Self::ToolFailed { id, error: output }
                } else {
                    Self::ToolFinished { id, result: output }
                }
            }
            Some("session/request_permission") => Self::PermissionRequested,
            Some("lato/session/compaction") => match params
                .get("event")
                .and_then(serde_json::Value::as_str)
            {
                Some("started") => params
                    .get("compactionId")
                    .and_then(serde_json::Value::as_str)
                    .map(|value| Self::CompactionStarted {
                        compaction_id: value.to_owned(),
                        trigger: params
                            .get("trigger")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("manual")
                            .to_owned(),
                    })
                    .unwrap_or(Self::Unknown),
                Some("completed") => {
                    let before = serde_json::from_value(params["before"].clone());
                    let after = serde_json::from_value(params["after"].clone());
                    let checkpoint_id = params
                        .get("checkpointId")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    match (before, after, checkpoint_id) {
                        (Ok(before), Ok(after), Some(checkpoint_id)) => Self::CompactionCompleted {
                            before,
                            after,
                            checkpoint_id,
                            warning: serde_json::from_value(params["warning"].clone()).ok(),
                        },
                        _ => Self::Unknown,
                    }
                }
                Some("failed") => Self::CompactionFailed {
                    code: params["error"]["code"]
                        .as_str()
                        .unwrap_or("compaction.failed")
                        .to_owned(),
                    message: params["error"]["message"]
                        .as_str()
                        .unwrap_or("compaction failed")
                        .to_owned(),
                },
                Some("cancelled") => Self::CompactionCancelled,
                _ => Self::Unknown,
            },
            Some("lato/session/context") => {
                let Some(estimated_input_tokens) = params
                    .get("estimatedInputTokens")
                    .and_then(serde_json::Value::as_u64)
                else {
                    return Self::Unknown;
                };
                let context_window = params
                    .get("contextWindow")
                    .and_then(serde_json::Value::as_u64)
                    .filter(|window| *window > 0);
                Self::ContextUsage {
                    estimated_input_tokens,
                    context_window,
                    utilization_percent: context_window.and_then(|_| {
                        params
                            .get("utilizationPercent")
                            .and_then(serde_json::Value::as_u64)
                            .and_then(|percent| u8::try_from(percent).ok())
                    }),
                }
            }
            Some("lato/session/recovery") => params
                .get("automaticCompactionSuppression")
                .and_then(serde_json::Value::as_str)
                .map(|value| Self::RecoverySuppression(value.to_owned()))
                .unwrap_or(Self::Unknown),
            Some("lato/session/model_changed") => {
                match (
                    params.get("provider").and_then(serde_json::Value::as_str),
                    params.get("model").and_then(serde_json::Value::as_str),
                ) {
                    (Some(provider), Some(model)) => Self::ModelChanged {
                        provider: provider.to_owned(),
                        model: model.to_owned(),
                        warning: serde_json::from_value(params["compactionWarning"].clone()).ok(),
                    },
                    _ => Self::Unknown,
                }
            }
            _ => Self::Unknown,
        }
    }
}

pub async fn run_prompt_over_acp_with_stream(
    cwd: std::path::PathBuf,
    lato_home: std::path::PathBuf,
    trust: SessionTrust,
    text: String,
    stream: Arc<dyn ModelStream>,
) -> Result<String, String> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut host = AcpHost::new_with_home(cwd, trust, tx, stream, lato_home);
    let _ = host
        .handle(req(1, "initialize", serde_json::json!({})))
        .await;
    let new = host
        .handle(req(2, "session/new", serde_json::json!({})))
        .await
        .ok_or("no response")?;
    let sid = new["result"]["sessionId"]
        .as_str()
        .ok_or("no session")?
        .to_string();
    let res = host
        .handle(req(
            3,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": text}),
        ))
        .await
        .ok_or("no response")?;
    if res.get("error").is_some() {
        return Err(res["error"]["message"]
            .as_str()
            .unwrap_or("error")
            .to_string());
    }
    debug_assert!(host.prompts_via_acp > 0);
    Ok(res["result"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string())
}

pub struct InteractiveAcpClient {
    host: AcpHost,
    updates: tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
    session_id: String,
    next_id: i32,
}

impl InteractiveAcpClient {
    pub async fn new_session_with_approval(
        cwd: std::path::PathBuf,
        lato_home: std::path::PathBuf,
        trust: SessionTrust,
        stream: Arc<dyn ModelStream>,
        approval: Option<Arc<dyn ToolApproval>>,
    ) -> Result<Self, String> {
        Self::initialize_with_approval(cwd, lato_home, trust, stream, approval, SessionStart::New)
            .await
    }

    pub async fn resume_session_with_approval(
        cwd: std::path::PathBuf,
        lato_home: std::path::PathBuf,
        trust: SessionTrust,
        stream: Arc<dyn ModelStream>,
        approval: Option<Arc<dyn ToolApproval>>,
        session_id: String,
    ) -> Result<Self, String> {
        Self::initialize_with_approval(
            cwd,
            lato_home,
            trust,
            stream,
            approval,
            SessionStart::Resume(session_id),
        )
        .await
    }

    async fn initialize_with_approval(
        cwd: std::path::PathBuf,
        lato_home: std::path::PathBuf,
        trust: SessionTrust,
        stream: Arc<dyn ModelStream>,
        approval: Option<Arc<dyn ToolApproval>>,
        start: SessionStart,
    ) -> Result<Self, String> {
        let (tx, updates) = tokio::sync::mpsc::unbounded_channel();
        let mut host =
            AcpHost::new_with_approval_and_home(cwd, trust, tx, stream, approval, lato_home);
        let _ = host
            .handle(req(1, "initialize", serde_json::json!({})))
            .await;
        let (method, params) = match start {
            SessionStart::New => ("session/new", serde_json::json!({})),
            SessionStart::Resume(session_id) => (
                "session/resume",
                serde_json::json!({"sessionId": session_id}),
            ),
        };
        let response = host
            .handle(req(2, method, params))
            .await
            .ok_or("no response")?;
        let result = response_result(&response)?;
        let session_id = result["sessionId"]
            .as_str()
            .ok_or("no session")?
            .to_string();
        Ok(Self {
            host,
            updates,
            session_id,
            next_id: 3,
        })
    }

    pub async fn send_streaming(
        &mut self,
        text: String,
        mut on_event: impl FnMut(&serde_json::Value),
    ) -> Result<String, String> {
        let id = self.take_id();
        let request = req(
            id,
            "session/prompt",
            serde_json::json!({"sessionId": self.session_id, "text": text}),
        );
        let host = &mut self.host;
        let updates = &mut self.updates;
        let mut response_future = Box::pin(host.handle(request));
        let response = loop {
            tokio::select! {
                response = &mut response_future => break response.ok_or("no response")?,
                event = updates.recv() => if let Some(event) = event { on_event(&event); },
            }
        };
        while let Ok(event) = updates.try_recv() {
            on_event(&event);
        }
        response_result_text(response)
    }

    pub async fn compact_streaming(
        &mut self,
        user_context: Option<String>,
        mut on_event: impl FnMut(&serde_json::Value),
    ) -> Result<CompactionResponse, String> {
        let id = self.take_id();
        let request = req(
            id,
            "lato/session/compact",
            serde_json::json!({
                "sessionId": self.session_id,
                "userContext": user_context,
            }),
        );
        let host = &mut self.host;
        let updates = &mut self.updates;
        let mut response_future = Box::pin(host.handle(request));
        let response = loop {
            tokio::select! {
                response = &mut response_future => break response.ok_or("no response")?,
                event = updates.recv() => if let Some(event) = event { on_event(&event); },
            }
        };
        while let Ok(event) = updates.try_recv() {
            on_event(&event);
        }
        serde_json::from_value(response_result(&response)?.clone())
            .map_err(|error| format!("invalid compaction response: {error}"))
    }

    pub async fn set_model(&mut self, selection: &str) -> Result<ModelSwitchResponse, String> {
        let (provider, model) = selection
            .split_once('/')
            .ok_or("model must be provider/model")?;
        let id = self.take_id();
        let response = self
            .host
            .handle(req(
                id,
                "session/set_model",
                serde_json::json!({
                    "sessionId": self.session_id,
                    "provider": provider,
                    "model": model,
                }),
            ))
            .await
            .ok_or("no response")?;
        let result = serde_json::from_value(response_result(&response)?.clone())
            .map_err(|error| error.to_string())?;
        while self.updates.try_recv().is_ok() {}
        Ok(result)
    }

    pub async fn clear(&mut self) -> Result<(), String> {
        let id = self.take_id();
        let response = self
            .host
            .handle(req(id, "session/new", serde_json::json!({})))
            .await
            .ok_or("no response")?;
        self.session_id = response["result"]["sessionId"]
            .as_str()
            .ok_or("no session")?
            .to_string();
        Ok(())
    }

    pub async fn resume(&mut self, session_id: String) -> Result<(), String> {
        let id = self.take_id();
        let response = self
            .host
            .handle(req(
                id,
                "session/resume",
                serde_json::json!({"sessionId": session_id}),
            ))
            .await
            .ok_or("no response")?;
        self.session_id = response_result(&response)?["sessionId"]
            .as_str()
            .ok_or("no session")?
            .to_string();
        Ok(())
    }

    pub async fn cancel(&mut self) -> Result<(), String> {
        let id = self.take_id();
        let response = self
            .host
            .handle(req(
                id,
                "session/cancel",
                serde_json::json!({"sessionId": self.session_id}),
            ))
            .await
            .ok_or("no response")?;
        response_result(&response).map(|_| ())
    }

    pub async fn list_session_summaries(&mut self) -> Result<Vec<SessionSummary>, String> {
        let id = self.take_id();
        let response = self
            .host
            .handle(req(id, "lato/session/list", serde_json::json!({})))
            .await
            .ok_or("no response")?;
        serde_json::from_value(response_result(&response)?["sessions"].clone())
            .map_err(|error| format!("invalid structured session list: {error}"))
    }

    pub async fn rename_session(
        &mut self,
        session_id: &str,
        title: &str,
    ) -> Result<SessionSummary, String> {
        let id = self.take_id();
        let response = self
            .host
            .handle(req(
                id,
                "lato/session/rename",
                serde_json::json!({"sessionId": session_id, "title": title}),
            ))
            .await
            .ok_or("no response")?;
        serde_json::from_value(response_result(&response)?.clone())
            .map_err(|error| format!("invalid rename response: {error}"))
    }

    pub async fn delete_session(&mut self, session_id: &str) -> Result<Option<String>, String> {
        let id = self.take_id();
        let response = self
            .host
            .handle(req(
                id,
                "lato/session/delete",
                serde_json::json!({"sessionId": session_id}),
            ))
            .await
            .ok_or("no response")?;
        response_result(&response)?;
        if session_id != self.session_id {
            return Ok(None);
        }
        self.clear().await?;
        Ok(Some(self.session_id.clone()))
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    fn take_id(&mut self) -> i32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

enum SessionStart {
    New,
    Resume(String),
}

#[allow(dead_code)]
pub async fn list_sessions_over_acp(cwd: std::path::PathBuf) -> Result<Vec<String>, String> {
    let (tx, _updates) = tokio::sync::mpsc::unbounded_channel();
    let trust = SessionTrust::for_headless_prompt(&cwd);
    let mut host = AcpHost::new(cwd, trust, tx, default_fake_stream());
    let _ = host
        .handle(req(1, "initialize", serde_json::json!({})))
        .await;
    let response = host
        .handle(req(2, "session/list", serde_json::json!({})))
        .await
        .ok_or("no response")?;
    response_result(&response)?["sessions"]
        .as_array()
        .ok_or_else(|| "invalid session list response".to_string())?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "invalid session id in response".to_string())
        })
        .collect()
}

pub async fn list_session_summaries_over_acp(
    cwd: std::path::PathBuf,
    lato_home: std::path::PathBuf,
) -> Result<Vec<SessionSummary>, String> {
    let result =
        call_session_extension(cwd, lato_home, "lato/session/list", serde_json::json!({})).await?;
    serde_json::from_value(result["sessions"].clone())
        .map_err(|error| format!("invalid structured session list: {error}"))
}

pub async fn rename_session_over_acp(
    cwd: std::path::PathBuf,
    lato_home: std::path::PathBuf,
    session_id: &str,
    title: &str,
) -> Result<SessionSummary, String> {
    let result = call_session_extension(
        cwd,
        lato_home,
        "lato/session/rename",
        serde_json::json!({"sessionId": session_id, "title": title}),
    )
    .await?;
    serde_json::from_value(result).map_err(|error| format!("invalid rename response: {error}"))
}

pub async fn delete_session_over_acp(
    cwd: std::path::PathBuf,
    lato_home: std::path::PathBuf,
    session_id: &str,
) -> Result<(), String> {
    call_session_extension(
        cwd,
        lato_home,
        "lato/session/delete",
        serde_json::json!({"sessionId": session_id}),
    )
    .await
    .map(|_| ())
}

async fn call_session_extension(
    cwd: std::path::PathBuf,
    lato_home: std::path::PathBuf,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (tx, _updates) = tokio::sync::mpsc::unbounded_channel();
    let trust = SessionTrust::for_headless_prompt(&cwd);
    let mut host = AcpHost::new_with_home(cwd, trust, tx, default_fake_stream(), lato_home);
    let _ = host
        .handle(req(1, "initialize", serde_json::json!({})))
        .await;
    let response = host
        .handle(req(2, method, params))
        .await
        .ok_or("no response")?;
    response_result(&response).cloned()
}

fn response_result(response: &serde_json::Value) -> Result<&serde_json::Value, String> {
    if response.get("error").is_some() {
        return Err(response["error"]["message"]
            .as_str()
            .unwrap_or("error")
            .to_string());
    }
    response
        .get("result")
        .ok_or_else(|| "no result".to_string())
}

fn response_result_text(response: serde_json::Value) -> Result<String, String> {
    Ok(response_result(&response)?["text"]
        .as_str()
        .unwrap_or_default()
        .to_string())
}

fn req(id: i32, method: &str, params: serde_json::Value) -> JsonRpcReq {
    JsonRpcReq {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::json!(id)),
        method: method.into(),
        params: Some(params),
    }
}

#[cfg(test)]
mod tests {
    use super::ClientUpdate;

    #[test]
    fn converts_tool_call_update() {
        let raw = serde_json::json!({
            "method": "session/tool_call",
            "params": {
                "id": "call-1",
                "name": "read_file",
                "arguments": {"path": "src/main.rs"}
            }
        });
        assert!(matches!(
            ClientUpdate::from_json(&raw),
            ClientUpdate::ToolStarted { id, name, .. }
                if id == "call-1" && name == "read_file"
        ));
    }

    #[test]
    fn converts_tool_result_status() {
        let done = serde_json::json!({
            "method": "session/tool_result",
            "params": {"id": "call-1", "status": "done", "result": "ok"}
        });
        let failed = serde_json::json!({
            "method": "session/tool_result",
            "params": {"id": "call-2", "status": "error", "result": "denied"}
        });
        assert_eq!(
            ClientUpdate::from_json(&done),
            ClientUpdate::ToolFinished {
                id: "call-1".into(),
                result: "ok".into()
            }
        );
        assert_eq!(
            ClientUpdate::from_json(&failed),
            ClientUpdate::ToolFailed {
                id: "call-2".into(),
                error: "denied".into()
            }
        );
    }

    #[test]
    fn converts_known_and_unknown_context_usage() {
        let known = serde_json::json!({
            "method":"lato/session/context",
            "params":{"sessionId":"s1","estimatedInputTokens":850,"contextWindow":1000,"utilizationPercent":85}
        });
        assert_eq!(
            ClientUpdate::from_json(&known),
            ClientUpdate::ContextUsage {
                estimated_input_tokens: 850,
                context_window: Some(1_000),
                utilization_percent: Some(85),
            }
        );
        let unknown = serde_json::json!({
            "method":"lato/session/context",
            "params":{"sessionId":"s1","estimatedInputTokens":850,"contextWindow":0,"utilizationPercent":0}
        });
        assert_eq!(
            ClientUpdate::from_json(&unknown),
            ClientUpdate::ContextUsage {
                estimated_input_tokens: 850,
                context_window: None,
                utilization_percent: None,
            }
        );
    }

    #[test]
    fn converts_automatic_compaction_suppression() {
        for value in ["none", "turn", "sticky", "until_success", "auth"] {
            let update = serde_json::json!({
                "method":"lato/session/recovery",
                "params":{"automaticCompactionSuppression":value}
            });
            assert_eq!(
                ClientUpdate::from_json(&update),
                ClientUpdate::RecoverySuppression(value.into())
            );
        }
    }

    #[tokio::test]
    async fn fresh_persisted_session_can_be_renamed_before_first_prompt() {
        use super::*;

        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let trust = SessionTrust::for_interactive(workspace.path(), false);
        let mut client = InteractiveAcpClient::new_session_with_approval(
            workspace.path().to_path_buf(),
            home.path().to_path_buf(),
            trust,
            default_fake_stream(),
            None,
        )
        .await
        .unwrap();
        let session_id = client.session_id().to_string();

        let renamed = client.rename_session(&session_id, "abc").await.unwrap();
        assert_eq!(renamed.session_id, session_id);
        assert_eq!(renamed.title, "abc");
        assert_eq!(renamed.title_source, "manual");
        assert!(
            home.path()
                .join("sessions")
                .join(&session_id)
                .join("metadata.json")
                .is_file()
        );
    }

    #[tokio::test]
    async fn model_switch_preserves_session_and_conversation_context() {
        use super::*;
        use lato_ai::{StreamPiece, SwitchableModelStream};
        struct RecordingStream {
            text: &'static str,
            contexts: std::sync::Mutex<Vec<serde_json::Value>>,
        }
        #[async_trait::async_trait]
        impl ModelStream for RecordingStream {
            async fn stream(
                &self,
                _: usize,
                context: serde_json::Value,
                tx: tokio::sync::mpsc::Sender<StreamPiece>,
            ) -> Result<(), lato_core::ModelError> {
                self.contexts.lock().unwrap().push(context);
                tx.send(StreamPiece::Text(self.text.into()))
                    .await
                    .map_err(|_| {
                        lato_core::ModelError::new(
                            "model.receiver_closed",
                            "model stream receiver closed",
                            lato_core::Retryability::Never,
                        )
                    })
            }
        }
        let workspace = tempfile::tempdir().unwrap();
        let trust = SessionTrust::for_interactive(workspace.path(), false);
        let first = Arc::new(RecordingStream {
            text: "first-answer",
            contexts: Default::default(),
        });
        let second = Arc::new(RecordingStream {
            text: "second-answer",
            contexts: Default::default(),
        });
        let switchable = Arc::new(SwitchableModelStream::from_stream(first));
        let mut client = InteractiveAcpClient::new_session_with_approval(
            workspace.path().to_path_buf(),
            workspace.path().join("home"),
            trust,
            switchable.clone(),
            None,
        )
        .await
        .unwrap();
        let original = client.session_id().to_string();
        assert_eq!(
            client
                .send_streaming("remember marker-saffron".into(), |_| {})
                .await
                .unwrap(),
            "first-answer"
        );
        switchable.set(second.clone()).await;
        assert_eq!(
            client
                .send_streaming("what did I say?".into(), |_| {})
                .await
                .unwrap(),
            "second-answer"
        );
        assert_eq!(client.session_id(), original);
        {
            let contexts = second.contexts.lock().unwrap();
            assert!(contexts[0].to_string().contains("marker-saffron"));
            assert!(contexts[0].to_string().contains("first-answer"));
        }
        client.clear().await.unwrap();
        assert_ne!(client.session_id(), original);
        client.resume(original.clone()).await.unwrap();
        assert_eq!(client.session_id(), original);
        assert!(client.resume("nonexistent".into()).await.is_err());
        assert_eq!(client.session_id(), original);
    }
}
