use lato_agent::{AcpHost, REQUIRED_SECTIONS, default_fake_stream};
use lato_ai::{FakeModelStream, StreamPiece};
use lato_core::{AgentProfile, BudgetAmount, BudgetLimits, ResultContract, TaskId, TaskScope};
use lato_protocol::JsonRpcReq;
use lato_runtime::{SpawnMode, SpawnTaskRequest, SubagentBackend};
use lato_workspace::SessionTrust;
use tokio_util::sync::CancellationToken;

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
    assert!(host.task_backend(&sid).is_some());
    host.handle(req(
        2,
        "session/close",
        serde_json::json!({"sessionId": sid}),
    ))
    .await
    .unwrap();
    assert!(host.task_backend(&sid).is_none());
    assert!(host.session_plugin_snapshot(&sid).await.is_none());
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
async fn plugins_reload_publishes_and_fans_out_a_new_generation() {
    let fixture = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let plugin = fixture.path().join(".lato/plugins/demo");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(plugin.join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
    let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut host = AcpHost::new_with_home(
        fixture.path().to_path_buf(),
        SessionTrust::for_interactive(fixture.path(), false),
        updates_tx,
        default_fake_stream(),
        home.path().to_path_buf(),
    );
    let sid = new_session(&mut host).await;
    let before = host
        .session_plugin_snapshot(&sid)
        .await
        .unwrap()
        .generation();
    let response = host
        .handle(req(
            2,
            "lato/plugins/reload",
            serde_json::json!({"force": true}),
        ))
        .await
        .unwrap();
    let generation = response["result"]["generation"].as_u64().unwrap();
    assert!(generation > before);
    assert_eq!(response["result"]["discovered"], 1);
    assert_eq!(response["result"]["active"], 0);
    assert_eq!(
        host.session_plugin_snapshot(&sid)
            .await
            .unwrap()
            .generation(),
        generation
    );
}

#[tokio::test]
async fn trusted_project_plugin_requires_persisted_enablement_to_be_active() {
    let fixture = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let plugin = fixture.path().join(".lato/plugins/demo");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(plugin.join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
    std::fs::write(
        home.path().join("config.json"),
        r#"{"plugins":{"enabled":["demo"],"disabled":[]}}"#,
    )
    .unwrap();
    let (updates_tx, _updates_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut host = AcpHost::new_with_home(
        fixture.path().to_path_buf(),
        SessionTrust::for_headless_prompt(fixture.path()),
        updates_tx,
        default_fake_stream(),
        home.path().to_path_buf(),
    );
    let sid = new_session(&mut host).await;
    assert_eq!(
        host.session_plugin_snapshot(&sid)
            .await
            .unwrap()
            .active_names(),
        vec!["demo"]
    );
}

#[tokio::test]
async fn task_lifecycle_is_exposed_as_additive_acp_events() {
    let (mut host, mut updates) = host();
    let sid = new_session(&mut host).await;
    let backend = host.task_backend(&sid).unwrap();
    backend
        .spawn(SpawnTaskRequest {
            task_id: TaskId::from("event-child"),
            scope: TaskScope {
                objective: "inspect the workspace".into(),
                context_refs: Vec::new(),
            },
            profile: AgentProfile::explorer(),
            requested_capabilities: None,
            budget: BudgetLimits::limited(BudgetAmount {
                input_tokens: 10_000,
                output_tokens: 2_000,
                total_tokens: 12_000,
                tool_calls: 16,
                cost_micros: 100_000,
                wall_time_ms: 10_000,
                retries: 1,
                child_tasks: 0,
                worktrees: 0,
            }),
            result_contract: ResultContract {
                schema: None,
                max_output_bytes: 4_096,
            },
            mode: SpawnMode::Background,
            cancellation: CancellationToken::new(),
        })
        .await
        .unwrap();
    let spawned = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let update = updates.recv().await.unwrap();
            if update["method"] == "lato/task/event"
                && update["params"]["session_id"] == sid
                && update["params"]["task_id"] == "event-child"
                && update["params"]["payload"]["type"] == "spawn_accepted"
            {
                return update;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(spawned["params"]["task_id"], "event-child");

    host.handle(req(
        2,
        "session/close",
        serde_json::json!({"sessionId": sid}),
    ))
    .await
    .unwrap();
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

fn healthy_summary() -> String {
    let detail =
        "preserve verified decisions, implementation evidence, and pending work ".repeat(2);
    REQUIRED_SECTIONS
        .iter()
        .enumerate()
        .map(|(index, heading)| format!("{}. {}: {detail}", index + 1, heading))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[tokio::test]
async fn acp_compact_returns_checkpoint_sizes_and_streams_lifecycle_updates() {
    let cwd = std::env::current_dir().unwrap();
    let (updates_tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let stream = std::sync::Arc::new(FakeModelStream::new(vec![
        vec![StreamPiece::Text("prior work ".repeat(2_000))],
        vec![StreamPiece::Text(healthy_summary())],
    ]));
    let mut host = AcpHost::new(
        cwd.clone(),
        SessionTrust::for_headless_prompt(&cwd),
        updates_tx,
        stream,
    );
    let sid = new_session(&mut host).await;
    let prompt = host
        .handle(req(
            2,
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "finish the parser"}),
        ))
        .await
        .unwrap();
    assert_eq!(prompt["result"]["status"], "complete");

    let compact = host
        .handle(req(
            3,
            "lato/session/compact",
            serde_json::json!({
                "sessionId": sid,
                "userContext": "preserve the parser root cause"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(compact["result"]["status"], "complete");
    assert!(
        compact["result"]["before"]["messageCount"]
            .as_u64()
            .unwrap()
            >= 3
    );
    assert!(compact["result"]["after"]["messageCount"].as_u64().unwrap() >= 3);
    assert!(compact["result"]["checkpointId"].as_str().is_some());

    let lifecycle = std::iter::from_fn(|| updates.try_recv().ok())
        .filter(|value| value["method"] == "lato/session/compaction")
        .map(|value| value["params"]["event"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(lifecycle, vec!["started", "completed"]);
}

#[tokio::test]
async fn acp_compact_preserves_typed_error_data() {
    let (mut host, _) = host();
    let sid = new_session(&mut host).await;
    let response = host
        .handle(req(
            2,
            "lato/session/compact",
            serde_json::json!({"sessionId": sid}),
        ))
        .await
        .unwrap();
    assert_eq!(
        response["error"]["data"]["code"],
        "compaction.nothing_to_compact"
    );
}
