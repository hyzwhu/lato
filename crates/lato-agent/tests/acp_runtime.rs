use lato_agent::{AcpHost, default_fake_stream};
use lato_protocol::JsonRpcReq;
use lato_workspace::SessionTrust;

fn req(id: i32, method: &str, params: serde_json::Value) -> JsonRpcReq {
    JsonRpcReq {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::json!(id)),
        method: method.into(),
        params: Some(params),
    }
}

fn host() -> (
    AcpHost,
    tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
) {
    let cwd = std::env::current_dir().unwrap();
    let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
    (
        AcpHost::new(
            cwd.clone(),
            SessionTrust::for_headless_prompt(&cwd),
            updates_tx,
            default_fake_stream(),
        ),
        updates_rx,
    )
}

async fn new_session(host: &mut AcpHost) -> String {
    host.handle(req(1, "session/new", serde_json::json!({})))
        .await
        .unwrap()["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn acp_host_source_routes_sessions_through_runtime_facade() {
    let source = include_str!("../src/host.rs");
    assert!(source.contains("HashMap<String, Arc<RuntimeSession>>"));
    assert!(!source.contains(".prompt(PromptKind::Start"));
}

#[tokio::test]
async fn acp_prompt_emits_delta_and_returns_the_same_final_text() {
    let (mut host, mut updates) = host();
    let sid = new_session(&mut host).await;
    let response = host
        .handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(response["result"]["status"], "complete");

    let mut deltas = String::new();
    while let Ok(update) = updates.try_recv() {
        if update["method"] == "session/update"
            && let Some(delta) = update["params"]["delta"].as_str()
        {
            deltas.push_str(delta);
        }
    }
    assert!(!deltas.is_empty());
    assert_eq!(response["result"]["text"], deltas);
}

#[tokio::test]
async fn acp_close_stops_and_removes_the_runtime_session() {
    let (mut host, _) = host();
    let sid = new_session(&mut host).await;
    host.handle(req(
        2,
        "session/close",
        serde_json::json!({"sessionId": sid}),
    ))
    .await
    .unwrap();
    let response = host
        .handle(req(
            3,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "after close"}),
        ))
        .await
        .unwrap();
    assert_eq!(response["error"]["message"], "unknown session");
}

#[tokio::test]
async fn idle_cancel_is_idempotent_and_session_remains_usable() {
    let (mut host, _) = host();
    let sid = new_session(&mut host).await;
    let cancelled = host
        .handle(req(
            2,
            "session/cancel",
            serde_json::json!({"sessionId": sid}),
        ))
        .await
        .unwrap();
    assert_eq!(cancelled["result"]["status"], "cancelled");
    let prompt = host
        .handle(req(
            3,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "still alive"}),
        ))
        .await
        .unwrap();
    assert_eq!(prompt["result"]["status"], "complete");
}

#[tokio::test]
async fn sequential_prompts_reuse_the_runtime_session_and_retain_history() {
    let (mut host, _) = host();
    let sid = new_session(&mut host).await;
    for (id, text) in [(2, "first"), (3, "second")] {
        let response = host
            .handle(req(
                id,
                "session/prompt",
                serde_json::json!({"sessionId": sid, "text": text}),
            ))
            .await
            .unwrap();
        assert_eq!(response["result"]["status"], "complete");
    }
    let listed = host
        .handle(req(4, "session/list", serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(
        listed["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|value| value.as_str() == Some(sid.as_str()))
            .count(),
        1,
    );
}
