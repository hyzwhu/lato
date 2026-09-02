use lato_agent::{AcpHost, ToolApproval, default_fake_stream};
use lato_ai::ModelStream;
use lato_protocol::JsonRpcReq;
use lato_workspace::SessionTrust;
use std::sync::Arc;

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
            _ => Self::Unknown,
        }
    }
}

pub async fn run_prompt_over_acp_with_stream(
    cwd: std::path::PathBuf,
    trust: SessionTrust,
    text: String,
    stream: Arc<dyn ModelStream>,
) -> Result<String, String> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut host = AcpHost::new(cwd, trust, tx, stream);
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
    pub async fn new_with_approval(
        cwd: std::path::PathBuf,
        trust: SessionTrust,
        stream: Arc<dyn ModelStream>,
        approval: Option<Arc<dyn ToolApproval>>,
    ) -> Result<Self, String> {
        Self::new_session_with_approval(cwd, trust, stream, approval).await
    }

    pub async fn new_session_with_approval(
        cwd: std::path::PathBuf,
        trust: SessionTrust,
        stream: Arc<dyn ModelStream>,
        approval: Option<Arc<dyn ToolApproval>>,
    ) -> Result<Self, String> {
        Self::initialize_with_approval(cwd, trust, stream, approval, SessionStart::New).await
    }

    pub async fn resume_session_with_approval(
        cwd: std::path::PathBuf,
        trust: SessionTrust,
        stream: Arc<dyn ModelStream>,
        approval: Option<Arc<dyn ToolApproval>>,
        session_id: String,
    ) -> Result<Self, String> {
        Self::initialize_with_approval(
            cwd,
            trust,
            stream,
            approval,
            SessionStart::Resume(session_id),
        )
        .await
    }

    async fn initialize_with_approval(
        cwd: std::path::PathBuf,
        trust: SessionTrust,
        stream: Arc<dyn ModelStream>,
        approval: Option<Arc<dyn ToolApproval>>,
        start: SessionStart,
    ) -> Result<Self, String> {
        let (tx, updates) = tokio::sync::mpsc::unbounded_channel();
        let mut host = AcpHost::new_with_approval(cwd, trust, tx, stream, approval);
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
}
