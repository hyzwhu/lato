use lato_agent::AcpHost;
use lato_ai::ModelStream;
use lato_protocol::JsonRpcReq;
use lato_workspace::SessionTrust;
use std::sync::Arc;

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
    session_id: String,
    next_id: i32,
}

impl InteractiveAcpClient {
    pub async fn new(
        cwd: std::path::PathBuf,
        trust: SessionTrust,
        stream: Arc<dyn ModelStream>,
    ) -> Result<Self, String> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut host = AcpHost::new(cwd, trust, tx, stream);
        let _ = host
            .handle(req(1, "initialize", serde_json::json!({})))
            .await;
        let response = host
            .handle(req(2, "session/new", serde_json::json!({})))
            .await
            .ok_or("no response")?;
        let session_id = response["result"]["sessionId"]
            .as_str()
            .ok_or("no session")?
            .to_string();
        Ok(Self {
            host,
            session_id,
            next_id: 3,
        })
    }

    pub async fn send(&mut self, text: String) -> Result<String, String> {
        let id = self.take_id();
        let response = self
            .host
            .handle(req(
                id,
                "session/prompt",
                serde_json::json!({"sessionId": self.session_id, "text": text}),
            ))
            .await
            .ok_or("no response")?;
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

    fn take_id(&mut self) -> i32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

fn response_result_text(response: serde_json::Value) -> Result<String, String> {
    if response.get("error").is_some() {
        return Err(response["error"]["message"]
            .as_str()
            .unwrap_or("error")
            .to_string());
    }
    Ok(response["result"]["text"]
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
