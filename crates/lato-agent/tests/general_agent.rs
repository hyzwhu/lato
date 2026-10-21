use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use lato_agent::{AcpHost, AgentConfiguration, ToolApproval};
use lato_ai::{FakeModelStream, ModelStream, StreamPiece};
use lato_core::{
    ApprovalRequest, ModelError, SideEffect, Tool, ToolCancellation, ToolCapability,
    ToolConcurrency, ToolContext, ToolDescriptor, ToolError, ToolIdempotency, ToolLayer, ToolName,
    ToolOutput, ToolSource,
};
use lato_protocol::JsonRpcReq;
use lato_workspace::SessionTrust;
use serde_json::{Value, json};
use tokio::sync::mpsc;

struct ProbeModel {
    inner: FakeModelStream,
    requests: Mutex<Vec<Value>>,
}

#[async_trait]
impl ModelStream for ProbeModel {
    async fn stream(
        &self,
        bytes: usize,
        context: Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError> {
        self.requests.lock().unwrap().push(context.clone());
        self.inner.stream(bytes, context, tx).await
    }
}

struct BusinessModule {
    name: &'static str,
    calls: Arc<AtomicUsize>,
    write: bool,
}

#[async_trait]
impl Tool for BusinessModule {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::parse(format!("business:{}", self.name)).unwrap(),
            version: "1.0.0".parse().unwrap(),
            description: format!("Business module {} for customer reports", self.name),
            input_schema: json!({"type":"object","properties":{"customer":{"type":"string"}},"required":["customer"],"additionalProperties":false}),
            capabilities: if self.write {
                vec![ToolCapability::NetworkWrite]
            } else {
                vec![ToolCapability::NetworkRead]
            },
            side_effect: if self.write {
                SideEffect::ExternalMutation
            } else {
                SideEffect::ReadOnly
            },
            concurrency: ToolConcurrency::Serial,
            idempotency: ToolIdempotency::Idempotent,
            timeout_ms: 1_000,
            max_output_bytes: 4_096,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource {
                layer: ToolLayer::User,
                id: "business-test".into(),
                replacement: None,
            },
        }
    }

    async fn invoke(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        assert!(context.execution_grant.is_some());
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            content: json!({"module":self.name,"customer":arguments["customer"],"revenue":42})
                .to_string(),
            metadata: json!({}),
            truncated: false,
            artifact_path: None,
        })
    }
}

fn config(tools: &[&str]) -> AgentConfiguration {
    serde_json::from_value(json!({"mode":"general","instructions":"Help the sales team using verified customer data.","tools":tools})).unwrap()
}

fn model(script: Vec<Vec<StreamPiece>>) -> Arc<ProbeModel> {
    Arc::new(ProbeModel {
        inner: FakeModelStream::new(script),
        requests: Mutex::new(Vec::new()),
    })
}

fn call(id: &str, name: &str, arguments: Value) -> Vec<StreamPiece> {
    vec![StreamPiece::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }]
}

fn host(home: &std::path::Path, stream: Arc<dyn ModelStream>, ask: bool) -> AcpHost {
    let (tx, _rx) = mpsc::unbounded_channel();
    let trust = if ask {
        SessionTrust::for_interactive(home, false)
    } else {
        SessionTrust::for_headless_prompt(home)
    };
    AcpHost::new_with_home(home.into(), trust, tx, stream, home.into())
}

async fn rpc(host: &mut AcpHost, method: &str, params: Value) -> Value {
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        host.handle(JsonRpcReq {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params: Some(params),
        }),
    )
    .await
    .expect("RPC timeout")
    .unwrap()
}

async fn start(host: &mut AcpHost) -> String {
    let response = rpc(host, "session/new", json!({})).await;
    response["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("{response}"))
        .into()
}

#[tokio::test]
async fn natural_language_drives_two_business_modules_with_only_selected_tools() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("AGENTS.md"),
        "CODING_ONLY_REPOSITORY_INSTRUCTIONS",
    )
    .unwrap();
    let stream = model(vec![
        call("lookup-1", "lookup_customer", json!({"customer":"Acme"})),
        call("report-1", "create_report", json!({"customer":"Acme"})),
        vec![StreamPiece::Text(
            "Acme report created using revenue 42.".into(),
        )],
    ]);
    let mut host = host(home.path(), stream.clone(), false);
    host.configure_agent(config(&[
        "business:lookup_customer",
        "business:create_report",
    ]))
    .unwrap();
    let lookup = Arc::new(AtomicUsize::new(0));
    let report = Arc::new(AtomicUsize::new(0));
    for (name, calls, write) in [
        ("lookup_customer", lookup.clone(), false),
        ("create_report", report.clone(), true),
    ] {
        host.register_business_tool(Arc::new(BusinessModule { name, calls, write }))
            .unwrap();
    }
    let sid = start(&mut host).await;
    let result = rpc(
        &mut host,
        "session/prompt",
        json!({"sessionId":sid,"text":"查一下 Acme 的情况并生成报告"}),
    )
    .await;
    assert_eq!(
        result["result"]["text"], "Acme report created using revenue 42.",
        "{result}"
    );
    assert_eq!(lookup.load(Ordering::SeqCst), 1);
    assert_eq!(report.load(Ordering::SeqCst), 1);
    {
        let requests = stream.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let system = requests[0]["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("general-purpose agent"));
        assert!(system.contains("sales team"));
        assert!(!system.contains("a coding agent"));
        assert!(!system.contains("CODING_ONLY_REPOSITORY_INSTRUCTIONS"));
        let names: Vec<_> = requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["create_report", "lookup_customer"]);
        assert!(requests[1]["messages"].to_string().contains("revenue"));
        assert!(
            requests[2]["messages"]
                .to_string()
                .contains("create_report")
        );
    }
    assert!(
        host.configure_agent(config(&[]))
            .unwrap_err()
            .contains("locked")
    );
    let info = rpc(&mut host, "lato/session/info", json!({})).await;
    assert_eq!(info["result"]["agent"]["mode"], "general");
    assert!(!info.to_string().contains("sales team"));
    rpc(&mut host, "session/close", json!({"sessionId":sid})).await;
}

#[tokio::test]
async fn forged_tools_and_invalid_business_arguments_have_zero_side_effects() {
    let home = tempfile::tempdir().unwrap();
    let stream = model(vec![
        call(
            "forged",
            "run_terminal_command",
            json!({"command":"touch forbidden"}),
        ),
        call(
            "canonical-forged",
            "builtin:run_terminal_command",
            json!({"command":"touch forbidden"}),
        ),
        call("wrong-schema", "lookup_customer", json!({"customer":42})),
        vec![StreamPiece::Text("Unable to run those operations.".into())],
    ]);
    let mut host = host(home.path(), stream.clone(), false);
    host.configure_agent(config(&["business:lookup_customer"]))
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    host.register_business_tool(Arc::new(BusinessModule {
        name: "lookup_customer",
        calls: calls.clone(),
        write: false,
    }))
    .unwrap();
    let sid = start(&mut host).await;
    let result = rpc(
        &mut host,
        "session/prompt",
        json!({"sessionId":sid,"text":"执行请求"}),
    )
    .await;
    assert!(result.get("error").is_none(), "{result}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!home.path().join("forbidden").exists());
    let requests = stream.requests.lock().unwrap();
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("tool.not_found")
    );
    assert!(
        requests[2]["messages"]
            .to_string()
            .contains("tool.not_found")
    );
    assert!(
        requests[3]["messages"]
            .to_string()
            .contains("lookup_customer.invalid_arguments")
    );
}

struct DenyApproval(Arc<AtomicUsize>);
#[async_trait]
impl ToolApproval for DenyApproval {
    async fn approve(&self, _: &ApprovalRequest) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        false
    }
}

#[tokio::test]
async fn business_writes_still_require_approval() {
    let home = tempfile::tempdir().unwrap();
    let stream = model(vec![
        call("write", "create_report", json!({"customer":"Acme"})),
        vec![StreamPiece::Text("Not approved.".into())],
    ]);
    let (tx, _rx) = mpsc::unbounded_channel();
    let approvals = Arc::new(AtomicUsize::new(0));
    let mut host = AcpHost::new_with_approval_and_home(
        home.path().into(),
        SessionTrust::for_interactive(home.path(), false),
        tx,
        stream,
        Some(Arc::new(DenyApproval(approvals.clone()))),
        home.path().into(),
    );
    host.configure_agent(config(&["business:create_report"]))
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    host.register_business_tool(Arc::new(BusinessModule {
        name: "create_report",
        calls: calls.clone(),
        write: true,
    }))
    .unwrap();
    let sid = start(&mut host).await;
    let result = rpc(
        &mut host,
        "session/prompt",
        json!({"sessionId":sid,"text":"生成报告"}),
    )
    .await;
    assert!(result.get("error").is_none(), "{result}");
    assert_eq!(approvals.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn persisted_general_session_requires_matching_configuration_on_resume() {
    let home = tempfile::tempdir().unwrap();
    let configuration = config(&[]);
    std::fs::write(
        home.path().join("agent.json"),
        serde_json::to_vec(&configuration).unwrap(),
    )
    .unwrap();
    let mut original = host(home.path(), model(vec![]), false);
    let sid = start(&mut original).await;
    let result = rpc(
        &mut original,
        "session/prompt",
        json!({"sessionId":sid,"text":"hello"}),
    )
    .await;
    assert!(result.get("error").is_none(), "{result}");
    rpc(&mut original, "session/close", json!({"sessionId":sid})).await;
    drop(original);
    let mut restored = host(home.path(), model(vec![]), false);
    let result = rpc(&mut restored, "session/resume", json!({"sessionId":sid})).await;
    assert!(result.get("error").is_none(), "{result}");
    rpc(&mut restored, "session/close", json!({"sessionId":sid})).await;
    drop(restored);
    let mut changed = configuration;
    changed.instructions = "Another application".into();
    std::fs::write(
        home.path().join("agent.json"),
        serde_json::to_vec(&changed).unwrap(),
    )
    .unwrap();
    let mut other = host(home.path(), model(vec![]), false);
    let result = rpc(&mut other, "session/resume", json!({"sessionId":sid})).await;
    assert!(
        result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("configuration_mismatch"),
        "{result}"
    );
    drop(other);
    std::fs::remove_file(home.path().join("agent.json")).unwrap();
    let mut missing = host(home.path(), model(vec![]), false);
    let result = rpc(&mut missing, "session/resume", json!({"sessionId":sid})).await;
    assert!(
        result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("configuration_mismatch"),
        "{result}"
    );
}

#[tokio::test]
async fn general_mode_does_not_force_coding_tools_and_hosts_keep_separate_identities() {
    for instruction in ["sales application", "support application"] {
        let home = tempfile::tempdir().unwrap();
        let stream = model(vec![vec![StreamPiece::Text(
            "No file tool is configured.".into(),
        )]]);
        let mut host = host(home.path(), stream.clone(), false);
        let mut configuration = config(&[]);
        configuration.instructions = instruction.into();
        host.configure_agent(configuration).unwrap();
        let sid = start(&mut host).await;
        let response = rpc(
            &mut host,
            "session/prompt",
            json!({"sessionId":sid,"text":"create report.txt"}),
        )
        .await;
        assert!(response.get("error").is_none(), "{response}");
        let requests = stream.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["tools"], json!([]));
        assert!(
            requests[0]["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains(instruction)
        );
    }
}

#[tokio::test]
async fn zero_turn_application_identity_is_persisted_and_default_coding_still_resumes() {
    let home = tempfile::tempdir().unwrap();
    let stream = model(vec![]);
    let mut original = host(home.path(), stream.clone(), false);
    let legacy_sid = start(&mut original).await;
    let result = rpc(
        &mut original,
        "session/prompt",
        json!({"sessionId":legacy_sid,"text":"hello"}),
    )
    .await;
    assert!(result.get("error").is_none(), "{result}");
    assert!(
        stream.requests.lock().unwrap()[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("a coding agent")
    );
    rpc(
        &mut original,
        "session/close",
        json!({"sessionId":legacy_sid}),
    )
    .await;
    let mut restored = host(home.path(), model(vec![]), false);
    let result = rpc(
        &mut restored,
        "session/resume",
        json!({"sessionId":legacy_sid}),
    )
    .await;
    assert!(result.get("error").is_none(), "{result}");
    rpc(
        &mut restored,
        "session/close",
        json!({"sessionId":legacy_sid}),
    )
    .await;

    std::fs::write(
        home.path().join("agent.json"),
        serde_json::to_vec(&config(&[])).unwrap(),
    )
    .unwrap();
    let mut general = host(home.path(), model(vec![]), false);
    let sid = start(&mut general).await;
    rpc(&mut general, "session/close", json!({"sessionId":sid})).await;
    std::fs::remove_file(home.path().join("agent.json")).unwrap();
    let mut missing = host(home.path(), model(vec![]), false);
    let result = rpc(&mut missing, "session/resume", json!({"sessionId":sid})).await;
    assert!(
        result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("configuration_mismatch"),
        "{result}"
    );
}

#[tokio::test]
async fn business_tool_names_take_precedence_over_legacy_coding_aliases() {
    let home = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let stream = model(vec![
        call("business-write", "write", json!({"customer":"Acme"})),
        vec![StreamPiece::Text("done".into())],
    ]);
    let mut host = host(home.path(), stream, false);
    host.configure_agent(config(&["business:write"])).unwrap();
    host.register_business_tool(Arc::new(BusinessModule {
        name: "write",
        calls: calls.clone(),
        write: false,
    }))
    .unwrap();
    let sid = start(&mut host).await;
    let result = rpc(
        &mut host,
        "session/prompt",
        json!({"sessionId":sid,"text":"Execute the business action"}),
    )
    .await;
    assert!(result.get("error").is_none(), "{result}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn bad_configuration_and_unknown_tools_fail_closed() {
    let home = tempfile::tempdir().unwrap();
    for contents in [
        "{",
        r#"{"mode":"general"}"#,
        r#"{"mode":"general","tools":["business:missing"]}"#,
    ] {
        std::fs::write(home.path().join("agent.json"), contents).unwrap();
        let stream = model(vec![]);
        let mut host = host(home.path(), stream.clone(), false);
        let result = rpc(&mut host, "session/new", json!({})).await;
        assert!(result.get("error").is_some(), "{result}");
        assert!(stream.requests.lock().unwrap().is_empty());
    }
}
