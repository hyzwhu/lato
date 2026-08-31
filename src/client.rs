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

fn req(id: i32, method: &str, params: serde_json::Value) -> JsonRpcReq {
    JsonRpcReq {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::json!(id)),
        method: method.into(),
        params: Some(params),
    }
}
