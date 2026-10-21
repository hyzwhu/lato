use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use lato_agent::{AgentConfiguration, AgentMode};
use lato_ai::{FakeModelStream, ModelStream, StreamPiece};
use lato_core::{
    ModelError, SideEffect, Tool, ToolCancellation, ToolCapability, ToolConcurrency, ToolContext,
    ToolDescriptor, ToolError, ToolIdempotency, ToolLayer, ToolName, ToolOutput, ToolSource,
};
use lato_service::{AgentService, ServiceConfig};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, mpsc},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "test-service-token-with-at-least-32-characters";

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

fn model(script: Vec<Vec<StreamPiece>>) -> Arc<ProbeModel> {
    Arc::new(ProbeModel {
        inner: FakeModelStream::new(script),
        requests: Mutex::new(Vec::new()),
    })
}

struct GateModel {
    entered: Semaphore,
    release: Semaphore,
}
#[async_trait]
impl ModelStream for GateModel {
    async fn stream(
        &self,
        _: usize,
        _: Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError> {
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        let _ = tx.send(StreamPiece::Text("finished".into())).await;
        Ok(())
    }
}

struct BusinessTool {
    name: &'static str,
    calls: Arc<AtomicUsize>,
    write: bool,
}
#[async_trait]
impl Tool for BusinessTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::parse(format!("business:{}", self.name)).unwrap(),
            version: "1.0.0".parse().unwrap(),
            description: format!("Business module {}", self.name),
            input_schema: json!({"type":"object","properties":{"customer":{"type":"string"}},"required":["customer"],"additionalProperties":false}),
            capabilities: vec![if self.write {
                ToolCapability::NetworkWrite
            } else {
                ToolCapability::NetworkRead
            }],
            side_effect: if self.write {
                SideEffect::ExternalMutation
            } else {
                SideEffect::ReadOnly
            },
            concurrency: ToolConcurrency::Serial,
            idempotency: ToolIdempotency::Idempotent,
            timeout_ms: 1000,
            max_output_bytes: 4096,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource {
                layer: ToolLayer::User,
                id: "service-test".into(),
                replacement: None,
            },
        }
    }
    async fn invoke(&self, context: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        assert!(context.execution_grant.is_some());
        assert_eq!(args["customer"], "Acme");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            content: json!({"customer":"Acme","revenue":42,"module":self.name}).to_string(),
            metadata: json!({}),
            truncated: false,
            artifact_path: None,
        })
    }
}

fn config(home: &Path, tools: Vec<Arc<dyn Tool>>) -> ServiceConfig {
    let mut config = ServiceConfig::new(home.into(), home.into(), TOKEN.into());
    config.agent = Some(AgentConfiguration {
        mode: AgentMode::General,
        instructions: "Help the business team using verified customer data.".into(),
        tools: Some(tools.iter().map(|t| t.descriptor().name).collect()),
        workspace_instructions: None,
    });
    config.tools = tools;
    config
}

struct Running {
    base: String,
    client: Client,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Running {
    async fn start(config: ServiceConfig, stream: Arc<dyn ModelStream>) -> Self {
        let service = AgentService::new(config, stream).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = CancellationToken::new();
        let task = tokio::spawn(service.serve(listener, stop.clone()));
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        Self {
            base,
            client,
            stop,
            task,
        }
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn session(&self) -> String {
        let response = self.post("/v1/sessions", json!({})).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        response.json::<Value>().await.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .into()
    }

    async fn submit(&self, id: &str, body: Value) -> String {
        let response = self
            .post(&format!("/v1/sessions/{id}/operations"), body)
            .await;
        let status = response.status();
        let body = response.json::<Value>().await.unwrap();
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        body["operationId"].as_str().unwrap().into()
    }

    async fn terminal(&self, id: &str) -> Value {
        timeout(Duration::from_secs(5), async {
            loop {
                let response = self.get(&format!("/v1/operations/{id}")).await;
                assert_eq!(response.status(), StatusCode::OK);
                let body: Value = response.json().await.unwrap();
                if matches!(
                    body["status"].as_str(),
                    Some("completed" | "cancelled" | "failed")
                ) {
                    return body;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("operation did not terminate")
    }

    async fn pending(&self, id: &str) -> Value {
        timeout(Duration::from_secs(3), async {
            loop {
                let body: Value = self
                    .get(&format!("/v1/sessions/{id}/approvals"))
                    .await
                    .json()
                    .await
                    .unwrap();
                if let Some(value) = body["approvals"].as_array().and_then(|v| v.first()) {
                    return value.clone();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("approval did not appear")
    }

    async fn finish(self) {
        self.stop.cancel();
        timeout(Duration::from_secs(14), self.task)
            .await
            .expect("service did not stop")
            .unwrap()
            .unwrap();
    }
}

fn call(id: &str, name: &str) -> Vec<StreamPiece> {
    vec![StreamPiece::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({"customer":"Acme"}),
    }]
}

async fn replay_to_completion(service: &Running, session: &str) -> String {
    replay_until(service, session, "operation_finished").await
}

async fn replay_until(service: &Running, session: &str, marker: &str) -> String {
    let mut response = service.get(&format!("/v1/sessions/{session}/events")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    timeout(Duration::from_secs(3), async {
        let mut text = String::new();
        while let Some(chunk) = response.chunk().await.unwrap() {
            text.push_str(&String::from_utf8_lossy(&chunk));
            if text.contains(marker) {
                return text;
            }
        }
        panic!("event stream ended before expected marker: {marker}");
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn asynchronous_operations_keep_status_creation_and_cancellation_responsive() {
    let home = tempfile::tempdir().unwrap();
    let model = Arc::new(GateModel {
        entered: Semaphore::new(0),
        release: Semaphore::new(0),
    });
    let service = Running::start(config(home.path(), vec![]), model.clone()).await;
    let session = service.session().await;
    let operation = service
        .submit(&session, json!({"text":"Help with a business task"}))
        .await;
    timeout(Duration::from_secs(3), model.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert_eq!(service.get("/v1/health").await.status(), StatusCode::OK);
    assert_eq!(
        service
            .get(&format!("/v1/operations/{operation}"))
            .await
            .status(),
        StatusCode::OK
    );
    let _other = service.session().await;
    assert_eq!(
        service
            .post(
                &format!("/v1/sessions/{session}/operations"),
                json!({"text":"duplicate"})
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let cancelled = service
        .post(&format!("/v1/sessions/{session}/cancel"), json!({}))
        .await;
    assert_eq!(cancelled.status(), StatusCode::OK);
    assert_eq!(service.terminal(&operation).await["status"], "cancelled");
    model.release.add_permits(2);
    let next = service
        .submit(&session, json!({"text":"Another business task"}))
        .await;
    assert_eq!(service.terminal(&next).await["status"], "completed");
    service.finish().await;
}

#[tokio::test]
async fn business_tool_chain_preserves_approval_and_replays_ordered_events() {
    let home = tempfile::tempdir().unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let stream = model(vec![
        call("lookup", "lookup_customer"),
        call("report", "create_report"),
        vec![StreamPiece::Text("Customer report created.".into())],
    ]);
    let service = Running::start(
        config(
            home.path(),
            vec![
                Arc::new(BusinessTool {
                    name: "lookup_customer",
                    calls: reads.clone(),
                    write: false,
                }),
                Arc::new(BusinessTool {
                    name: "create_report",
                    calls: writes.clone(),
                    write: true,
                }),
            ],
        ),
        stream.clone(),
    )
    .await;
    let session = service.session().await;
    let operation = service
        .submit(&session, json!({"text":"查询 Acme 并生成报告"}))
        .await;
    let pending = service.pending(&session).await;
    assert_eq!(pending["tool"], "business:create_report");
    assert_eq!(pending["arguments"], json!({"customer":"Acme"}));
    assert_eq!(pending["argumentsTruncated"], false);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    let approval = pending["approvalId"].as_str().unwrap();
    let other = service.session().await;
    assert_eq!(
        service
            .post(
                &format!("/v1/sessions/{other}/approvals/{approval}"),
                json!({"allow":true})
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        service
            .post(
                &format!("/v1/sessions/{session}/approvals/{approval}"),
                json!({"allow":true})
            )
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        service
            .post(
                &format!("/v1/sessions/{session}/approvals/{approval}"),
                json!({"allow":true})
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let result = service.terminal(&operation).await;
    assert_eq!(result["status"], "completed", "{result}");
    assert_eq!(result["result"]["text"], "Customer report created.");
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    let replay = replay_to_completion(&service, &session).await;
    assert!(replay.contains("approval_requested"));
    assert!(replay.contains("session/tool_result"));
    assert!(
        replay.find("session/tool_result").unwrap() < replay.find("operation_finished").unwrap()
    );
    let data: Vec<Value> = replay
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect();
    assert!(
        data.windows(2)
            .all(|w| w[0]["sequence"].as_u64() < w[1]["sequence"].as_u64())
    );
    let last = data.last().unwrap()["sequence"].as_u64().unwrap();
    let mut reconnect = service
        .client
        .get(format!("{}/v1/sessions/{session}/events", service.base))
        .bearer_auth(TOKEN)
        .header("Last-Event-ID", (last - 1).to_string())
        .send()
        .await
        .unwrap();
    let chunk = reconnect.chunk().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&chunk).contains("operation_finished"));
    drop(reconnect);
    assert_eq!(stream.requests.lock().unwrap().len(), 3);
    service.finish().await;
}

#[tokio::test]
async fn denied_and_expired_approvals_never_execute_business_writes() {
    for explicit_denial in [true, false] {
        let home = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let stream = model(vec![
            call("write", "create_report"),
            vec![StreamPiece::Text("Operation was not approved.".into())],
        ]);
        let mut configuration = config(
            home.path(),
            vec![Arc::new(BusinessTool {
                name: "create_report",
                calls: calls.clone(),
                write: true,
            })],
        );
        configuration.approval_timeout = Duration::from_millis(200);
        let service = Running::start(configuration, stream.clone()).await;
        let session = service.session().await;
        let operation = service
            .submit(&session, json!({"text":"Generate report"}))
            .await;
        let approval = service.pending(&session).await;
        if explicit_denial {
            let id = approval["approvalId"].as_str().unwrap();
            assert_eq!(
                service
                    .post(
                        &format!("/v1/sessions/{session}/approvals/{id}"),
                        json!({"allow":false})
                    )
                    .await
                    .status(),
                StatusCode::OK
            );
        }
        assert_eq!(service.terminal(&operation).await["status"], "completed");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            stream.requests.lock().unwrap()[1]["messages"]
                .to_string()
                .contains("policy.approval_denied")
        );
        let pending: Value = service
            .get(&format!("/v1/sessions/{session}/approvals"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(pending["approvals"], json!([]));
        service.finish().await;
    }
}

#[tokio::test]
async fn dropped_clients_do_not_cancel_admitted_work_and_shutdown_cancels_pending_work() {
    let home = tempfile::tempdir().unwrap();
    let model = Arc::new(GateModel {
        entered: Semaphore::new(0),
        release: Semaphore::new(0),
    });
    let service = Running::start(config(home.path(), vec![]), model.clone()).await;
    let session = service.session().await;
    let events = service.get(&format!("/v1/sessions/{session}/events")).await;
    drop(events);
    let response = service
        .post(
            &format!("/v1/sessions/{session}/operations"),
            json!({"text":"Do a task"}),
        )
        .await;
    drop(response);
    timeout(Duration::from_secs(3), model.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let status: Value = service
        .get(&format!("/v1/sessions/{session}"))
        .await
        .json()
        .await
        .unwrap();
    let operation = status["activeOperationId"].as_str().unwrap();
    model.release.add_permits(1);
    assert_eq!(service.terminal(operation).await["status"], "completed");
    let _next = service
        .submit(&session, json!({"text":"Wait for cancellation"}))
        .await;
    timeout(Duration::from_secs(3), model.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    service.finish().await;
}

#[tokio::test]
async fn authentication_strict_inputs_and_event_gaps_are_explicit() {
    let home = tempfile::tempdir().unwrap();
    let stream = model(vec![
        (0..650)
            .map(|_| StreamPiece::Text("delta".into()))
            .collect(),
    ]);
    let service = Running::start(config(home.path(), vec![]), stream).await;
    for token in [None, Some("incorrect")] {
        let mut request = service.client.get(format!("{}/v1/health", service.base));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "service.unauthorized"
        );
    }
    assert_eq!(
        service.get("/unknown").await.status(),
        StatusCode::NOT_FOUND
    );
    let invalid_path = service.get("/v1/sessions/%FF").await;
    assert_eq!(invalid_path.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        invalid_path.json::<Value>().await.unwrap()["error"]["code"],
        "service.invalid_request"
    );
    let session = service.session().await;
    for body in [
        json!({"text":""}),
        json!({"text":"hi","extra":"secret"}),
        json!({"text":"hi","skill":{"name":"x"}}),
    ] {
        assert!(
            service
                .post(&format!("/v1/sessions/{session}/operations"), body)
                .await
                .status()
                .is_client_error()
        );
    }
    let oversized = service
        .post(
            &format!("/v1/sessions/{session}/operations"),
            json!({"text":"x".repeat(70*1024)}),
        )
        .await;
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        oversized.json::<Value>().await.unwrap()["error"]["code"],
        "service.invalid_request"
    );
    let operation = service
        .submit(&session, json!({"text":"Stream data"}))
        .await;
    assert_eq!(service.terminal(&operation).await["status"], "completed");
    let expired = service
        .get(&format!("/v1/sessions/{session}/events?after=0"))
        .await;
    assert_eq!(expired.status(), StatusCode::CONFLICT);
    assert_eq!(
        expired.json::<Value>().await.unwrap()["error"]["code"],
        "service.event_gap"
    );
    let future = service
        .get(&format!("/v1/sessions/{session}/events?after={}", u64::MAX))
        .await;
    assert_eq!(future.status(), StatusCode::BAD_REQUEST);
    assert!(
        replay_to_completion(&service, &session)
            .await
            .contains("operation_finished")
    );
    service.finish().await;
}

#[tokio::test]
async fn session_journal_survives_service_restart_and_operations_are_not_retried() {
    let home = tempfile::tempdir().unwrap();
    let first = Running::start(
        config(home.path(), vec![]),
        model(vec![vec![StreamPiece::Text("first answer".into())]]),
    )
    .await;
    let session = first.session().await;
    let operation = first
        .submit(&session, json!({"text":"first question"}))
        .await;
    assert_eq!(first.terminal(&operation).await["status"], "completed");
    assert_eq!(
        first
            .post(&format!("/v1/sessions/{session}/close"), json!({}))
            .await
            .status(),
        StatusCode::OK
    );
    first.finish().await;
    let stream = model(vec![vec![StreamPiece::Text("second answer".into())]]);
    let second = Running::start(config(home.path(), vec![]), stream.clone()).await;
    assert_eq!(
        second
            .get(&format!("/v1/operations/{operation}"))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let resume = second
        .post("/v1/sessions/resume", json!({"sessionId":session}))
        .await;
    let status = resume.status();
    let body = resume.json::<Value>().await.unwrap();
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let operation = second
        .submit(&session, json!({"text":"second question"}))
        .await;
    assert_eq!(
        second.terminal(&operation).await["result"]["text"],
        "second answer"
    );
    assert!(
        stream.requests.lock().unwrap()[0]["messages"]
            .to_string()
            .contains("first question")
    );
    second.finish().await;
}

#[tokio::test]
async fn cancellation_denies_pending_approval_and_keeps_the_session_reusable() {
    let home = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let service = Running::start(
        config(
            home.path(),
            vec![Arc::new(BusinessTool {
                name: "create_report",
                calls: calls.clone(),
                write: true,
            })],
        ),
        model(vec![
            call("write", "create_report"),
            vec![StreamPiece::Text("Next task".into())],
        ]),
    )
    .await;
    let session = service.session().await;
    let operation = service
        .submit(&session, json!({"text":"Create a report"}))
        .await;
    let pending = service.pending(&session).await;
    assert_eq!(
        service
            .post(&format!("/v1/sessions/{session}/cancel"), json!({}))
            .await
            .status(),
        StatusCode::OK
    );
    let result = service.terminal(&operation).await;
    assert_eq!(result["status"], "cancelled", "{result}");
    let approval = pending["approvalId"].as_str().unwrap();
    assert_eq!(
        service
            .post(
                &format!("/v1/sessions/{session}/approvals/{approval}"),
                json!({"allow":true})
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let next = service
        .submit(&session, json!({"text":"Continue with another task"}))
        .await;
    assert_eq!(service.terminal(&next).await["status"], "completed");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    service.finish().await;
}

#[tokio::test]
async fn global_admission_rejects_excess_running_operations() {
    let home = tempfile::tempdir().unwrap();
    let gate = Arc::new(GateModel {
        entered: Semaphore::new(0),
        release: Semaphore::new(0),
    });
    let service = Running::start(config(home.path(), vec![]), gate.clone()).await;
    for _ in 0..16 {
        let session = service.session().await;
        service
            .submit(&session, json!({"text":"Wait for data"}))
            .await;
    }
    let session = service.session().await;
    let response = service
        .post(
            &format!("/v1/sessions/{session}/operations"),
            json!({"text":"One more task"}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "service.capacity"
    );
    service.finish().await;
}

#[tokio::test]
async fn large_results_and_events_report_truncation_at_utf8_boundaries() {
    let home = tempfile::tempdir().unwrap();
    let answer = "报".repeat(120_000);
    let original = answer.len();
    let service = Running::start(
        config(home.path(), vec![]),
        model(vec![vec![StreamPiece::Text(answer)]]),
    )
    .await;
    let session = service.session().await;
    let operation = service
        .submit(&session, json!({"text":"Return a long report"}))
        .await;
    let result = service.terminal(&operation).await;
    assert_eq!(result["status"], "completed", "{result}");
    assert_eq!(result["result"]["truncated"], true);
    assert_eq!(result["result"]["originalBytes"], original);
    let text = result["result"]["text"].as_str().unwrap();
    assert!(text.len() <= 256 * 1024);
    assert!(text.chars().all(|c| c == '报'));
    let replay = replay_to_completion(&service, &session).await;
    assert!(replay.contains("eventTruncated"));
    assert!(replay.contains(&operation));
    service.finish().await;
}

#[tokio::test]
async fn explicit_and_model_selected_skills_use_the_application_catalog() {
    let home = tempfile::tempdir().unwrap();
    let plugin = home.path().join("plugin");
    let skill = plugin.join("skills/report");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        plugin.join("plugin.json"),
        r#"{"name":"business-demo","skills":"skills"}"#,
    )
    .unwrap();
    std::fs::write(skill.join("SKILL.md"),"---\nname: report\ndescription: Report on a customer\n---\nBUSINESS_REPORT_INSTRUCTIONS customer=$ARGUMENTS\n").unwrap();
    std::fs::write(
        home.path().join("config.json"),
        r#"{"plugins":{"enabled":["business-demo"]}}"#,
    )
    .unwrap();
    let mut configuration = config(home.path(), vec![]);
    configuration.plugin_dirs = vec![plugin];
    configuration.trust = lato_workspace::SessionTrust::for_interactive(home.path(), true);
    configuration.agent.as_mut().unwrap().tools =
        Some(vec![ToolName::parse("builtin:skill").unwrap()]);
    let stream = model(vec![
        vec![StreamPiece::ToolCall {
            id: "invoke-skill".into(),
            name: "skill".into(),
            arguments: json!({"skill":"business-demo:report","args":"Acme"}),
        }],
        vec![StreamPiece::Text("Report ready".into())],
        vec![StreamPiece::Text("Explicit report ready".into())],
    ]);
    let service = Running::start(configuration, stream.clone()).await;
    let session = service.session().await;
    let skills: Value = service
        .get(&format!("/v1/sessions/{session}/skills"))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        skills["skills"]
            .to_string()
            .contains("business-demo:report"),
        "{skills}"
    );
    let natural = service
        .submit(
            &session,
            json!({"text":"Create a customer report for Acme"}),
        )
        .await;
    assert_eq!(service.terminal(&natural).await["status"], "completed");
    let explicit = service
        .submit(
            &session,
            json!({"skill":{"name":"business-demo:report","args":"Acme"}}),
        )
        .await;
    let result = service.terminal(&explicit).await;
    assert_eq!(result["status"], "completed", "{result}");
    {
        let requests = stream.requests.lock().unwrap();
        assert!(
            requests[1]["messages"]
                .to_string()
                .contains("BUSINESS_REPORT_INSTRUCTIONS"),
            "{}",
            requests[1]["messages"]
        );
        assert!(
            requests[2]["messages"]
                .to_string()
                .contains("customer=Acme")
        );
        assert_eq!(requests[0]["tools"].as_array().unwrap().len(), 1);
    }
    service.finish().await;
}

/// Main operation finishes while delegated calls still need decisions. Route by
/// the owner, preserve execution identity, and expose the final business value.
struct WorkflowModel {
    requests: Mutex<Vec<Value>>,
}
#[async_trait]
impl ModelStream for WorkflowModel {
    async fn stream(
        &self,
        _: usize,
        context: Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError> {
        self.requests.lock().unwrap().push(context.clone());
        let messages = context["messages"].as_array().unwrap();
        let delegated = messages.iter().any(|m| {
            m["role"] == "system"
                && m["content"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Delegated task context:")
        });
        let tools: Vec<_> = messages.iter().filter(|m| m["role"] == "tool").collect();
        let piece = if delegated {
            let all = context.to_string();
            assert!(all.contains("Help the business team using verified customer data."));
            let catalog = context["tools"].to_string();
            assert!(!catalog.contains("\"name\":\"workflow\""));
            assert!(!catalog.contains("\"name\":\"bash\""));
            if tools.is_empty() {
                StreamPiece::ToolCall {
                    id: "child-write".into(),
                    name: "business:report".into(),
                    arguments: json!({"customer":"Acme"}),
                }
            } else {
                StreamPiece::Text("```json\n{\"customer\":\"Acme\",\"revenue\":42}\n```".into())
            }
        } else if tools.is_empty() {
            StreamPiece::ToolCall {
                id: "wf-list".into(),
                name: "workflow".into(),
                arguments: json!({"action":"list"}),
            }
        } else if tools.len() == 1 {
            let list: Value = serde_json::from_str(tools[0]["content"].as_str().unwrap()).unwrap();
            let entry = &list["workflows"][0];
            StreamPiece::ToolCall {
                id: "wf-start".into(),
                name: "workflow".into(),
                arguments: json!({"action":"start","name":entry["id"],"revision":entry["revision"],"agentBudget":2}),
            }
        } else {
            StreamPiece::Text("Business workflow started.".into())
        };
        tx.send(piece).await.map_err(|_| ModelError::cancelled())?;
        Ok(())
    }
}

#[tokio::test]
async fn workflow_inherits_application_and_routes_child_approval_to_owner() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("workflows")).unwrap();
    std::fs::write(
        home.path().join("workflows/report.rhai"),
        r#"
        let meta = #{ name: "report", description: "customer reporting" };
        let r = agent("Create the Acme business report", #{output_schema: #{
            type:"object", properties:#{customer:#{type:"string"},revenue:#{type:"number"}},
            required:["customer","revenue"], additionalProperties:false
        }});
        if !r.success { throw "Business worker failed"; }
        complete(r.output);
    "#,
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(BusinessTool {
        name: "report",
        calls: calls.clone(),
        write: true,
    });
    let mut cfg = config(home.path(), vec![tool]);
    cfg.agent
        .as_mut()
        .unwrap()
        .tools
        .as_mut()
        .unwrap()
        .push(ToolName::parse("builtin:workflow").unwrap());
    let model = Arc::new(WorkflowModel {
        requests: Mutex::new(Vec::new()),
    });
    let service = Running::start(cfg, model.clone()).await;
    let session = service.session().await;
    let other = service.session().await;
    let discovery: Value = service
        .get(&format!("/v1/sessions/{session}/workflows"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(discovery["workflows"][0]["name"], "report");
    let op = service
        .submit(
            &session,
            json!({"text":"Prepare Acme report using a workflow"}),
        )
        .await;
    for action in ["list", "start"] {
        let decision = service.pending(&session).await;
        assert_eq!(decision["tool"], "builtin:workflow");
        assert_eq!(decision["arguments"]["action"], action);
        assert_eq!(
            service
                .post(
                    &format!(
                        "/v1/sessions/{session}/approvals/{}",
                        decision["approvalId"].as_str().unwrap()
                    ),
                    json!({"allow":true})
                )
                .await
                .status(),
            StatusCode::OK
        );
    }
    assert_eq!(service.terminal(&op).await["status"], "completed");
    let child = service.pending(&session).await;
    assert_eq!(child["sessionId"], session);
    assert_ne!(child["executionSessionId"], session);
    assert_eq!(child["tool"], "business:report");
    assert_eq!(child["arguments"], json!({"customer":"Acme"}));
    let approval = child["approvalId"].as_str().unwrap();
    assert_eq!(
        service
            .post(
                &format!("/v1/sessions/{other}/approvals/{approval}"),
                json!({"allow":true})
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        service
            .post(
                &format!("/v1/sessions/{session}/approvals/{approval}"),
                json!({"allow":true})
            )
            .await
            .status(),
        StatusCode::OK
    );
    let run = timeout(Duration::from_secs(8), async {
        loop {
            let runs: Value = service
                .get(&format!("/v1/sessions/{session}/workflow-runs"))
                .await
                .json()
                .await
                .unwrap();
            if runs["runs"][0]["status"] != "active" {
                break runs["runs"][0].clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(run["status"], "complete", "{run}");
    assert_eq!(run["result"], json!({"customer":"Acme","revenue":42}));
    assert_eq!(run["resultTruncated"], false);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(run["agentsUsed"], 1);
    let events = replay_until(&service, &session, "lato/delegated/update").await;
    assert!(events.contains("lato/delegated/update"), "{events}");
    assert_eq!(
        service
            .post(
                &format!("/v1/sessions/{session}/workflow-runs/resume"),
                json!({"name":"report","agentBudget":0})
            )
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        service
            .post(
                &format!("/v1/sessions/{session}/workflow-runs/pause"),
                json!({"name":"absent"})
            )
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    service.finish().await;
}

#[tokio::test]
async fn workflow_pause_resume_stop_cleans_up_delegated_approvals() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("workflows")).unwrap();
    std::fs::write(
        home.path().join("workflows/report.rhai"),
        r#"
        let meta = #{name:"report",description:"d"};
        let r=agent("Prepare report"); complete(r.output);
    "#,
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut cfg = config(
        home.path(),
        vec![Arc::new(BusinessTool {
            name: "report",
            calls: calls.clone(),
            write: true,
        })],
    );
    cfg.agent
        .as_mut()
        .unwrap()
        .tools
        .as_mut()
        .unwrap()
        .push(ToolName::parse("builtin:workflow").unwrap());
    let service = Running::start(
        cfg,
        Arc::new(WorkflowModel {
            requests: Mutex::new(Vec::new()),
        }),
    )
    .await;
    let session = service.session().await;
    let op = service
        .submit(&session, json!({"text":"Start report"}))
        .await;
    for _ in 0..2 {
        let pending = service.pending(&session).await;
        assert_eq!(pending["tool"], "builtin:workflow");
        assert_eq!(
            service
                .post(
                    &format!(
                        "/v1/sessions/{session}/approvals/{}",
                        pending["approvalId"].as_str().unwrap()
                    ),
                    json!({"allow":true})
                )
                .await
                .status(),
            StatusCode::OK
        );
    }
    assert_eq!(service.terminal(&op).await["status"], "completed");
    let original = service.pending(&session).await;
    assert_eq!(original["tool"], "business:report");
    assert_eq!(
        service
            .post(
                &format!("/v1/sessions/{session}/workflow-runs/pause"),
                json!({"name":"report"})
            )
            .await
            .status(),
        StatusCode::OK
    );
    timeout(Duration::from_secs(8), async {
        loop {
            let body: Value = service
                .get(&format!("/v1/sessions/{session}/workflow-runs"))
                .await
                .json()
                .await
                .unwrap();
            if body["runs"][0]["status"] == "user_paused" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        service
            .post(
                &format!(
                    "/v1/sessions/{session}/approvals/{}",
                    original["approvalId"].as_str().unwrap()
                ),
                json!({"allow":true})
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        service
            .post(
                &format!("/v1/sessions/{session}/workflow-runs/resume"),
                json!({"name":"report","agentBudget":2})
            )
            .await
            .status(),
        StatusCode::OK
    );
    let resumed = service.pending(&session).await;
    assert_eq!(resumed["tool"], "business:report");
    assert_ne!(resumed["approvalId"], original["approvalId"]);
    let response = service
        .post(
            &format!("/v1/sessions/{session}/workflow-runs/stop"),
            json!({"name":"report"}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap()["run"]["status"],
        "cancelled"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let pending: Value = service
        .get(&format!("/v1/sessions/{session}/approvals"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(pending["approvals"], json!([]));
    service.finish().await;
}
