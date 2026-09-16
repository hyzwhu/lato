//! T3 — Plan-mode command edges over the ACP surface (spec §2/§5).
//!
//! Proves the human command contract end to end: submit/approve legality,
//! model-inert phases, illegal edges refused, and a fresh activation that
//! never reuses an old approval.

use lato_agent::{AcpHost, default_fake_stream};
use lato_protocol::JsonRpcReq;
use lato_workspace::SessionTrust;
use serde_json::json;

fn req(id: i64, method: &str, params: serde_json::Value) -> JsonRpcReq {
    JsonRpcReq {
        jsonrpc: "2.0".into(),
        id: Some(json!(id)),
        method: method.to_string(),
        params: Some(params),
    }
}

fn host(cwd: &std::path::Path) -> AcpHost {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let trust = SessionTrust::for_headless_prompt(cwd);
    AcpHost::new_with_home(
        cwd.to_path_buf(),
        trust,
        tx,
        default_fake_stream(),
        cwd.join(".lato-home"),
    )
}

async fn plan_with_sid(host: &mut AcpHost, id: i64, method: &str, sid: &str) -> serde_json::Value {
    host.handle(req(id, method, json!({"sessionId": sid})))
        .await
        .expect("rpc response")
}

#[tokio::test]
async fn plan_command_edges_follow_the_frozen_state_machine() {
    let workspace = tempfile::tempdir().unwrap();
    let mut host = host(workspace.path());

    // session/new then a first status: inactive.
    let new = host.handle(req(0, "session/new", json!({}))).await.unwrap();
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();

    let status = plan_with_sid(&mut host, 1, "lato/plan/status", &sid).await;
    assert_eq!(status["result"]["phase"], "inactive");

    // Illegal edges from Inactive: submit and approve are refused.
    let submit = plan_with_sid(&mut host, 2, "lato/plan/submit", &sid).await;
    assert!(
        submit.get("error").is_some(),
        "submit from Inactive must fail"
    );
    let approve = plan_with_sid(&mut host, 3, "lato/plan/approve", &sid).await;
    assert!(
        approve.get("error").is_some(),
        "approve from Inactive must fail"
    );

    // Enter: Inactive → Drafting.
    let enter = plan_with_sid(&mut host, 4, "lato/plan/enter", &sid).await;
    assert_eq!(enter["result"]["phase"], "drafting");

    // Approve before submit is an illegal edge.
    let approve = plan_with_sid(&mut host, 5, "lato/plan/approve", &sid).await;
    assert!(
        approve.get("error").is_some(),
        "approve from Drafting must fail"
    );

    // Submit without a written draft is allowed, then approve records it.
    let submit = plan_with_sid(&mut host, 6, "lato/plan/submit", &sid).await;
    assert_eq!(submit["result"]["phase"], "awaiting_approval");

    std::fs::write(workspace.path().join("plan.md"), "# the plan").unwrap();
    let approve = plan_with_sid(&mut host, 7, "lato/plan/approve", &sid).await;
    assert_eq!(approve["result"]["phase"], "approved");
    assert_eq!(
        approve["result"]["approval"]["generation"].as_u64(),
        Some(1)
    );

    // Double approve is an illegal edge.
    let approve_again = plan_with_sid(&mut host, 8, "lato/plan/approve", &sid).await;
    assert!(approve_again.get("error").is_some());

    // Exit is legal from any state and lands in the audit terminal state.
    let exit = plan_with_sid(&mut host, 9, "lato/plan/exit", &sid).await;
    assert_eq!(exit["result"]["phase"], "exited");

    // A fresh activation increments the activation and never reuses the old
    // approval.
    let reenter = plan_with_sid(&mut host, 10, "lato/plan/enter", &sid).await;
    assert_eq!(reenter["result"]["phase"], "drafting");
    assert_eq!(reenter["result"]["activation"].as_u64(), Some(2));
    assert!(reenter["result"]["approval"].is_null());
    let status = plan_with_sid(&mut host, 11, "lato/plan/status", &sid).await;
    assert_eq!(status["result"]["activation"].as_u64(), Some(2));
    assert!(status["result"]["approval"].is_null());
}

#[tokio::test]
async fn submit_and_approve_reject_drafts_that_fail_closed() {
    let workspace = tempfile::tempdir().unwrap();
    let mut host = host(workspace.path());
    let new = host.handle(req(0, "session/new", json!({}))).await.unwrap();
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();

    plan_with_sid(&mut host, 1, "lato/plan/enter", &sid).await;

    // An oversized draft (131,073 bytes) cannot be submitted.
    std::fs::write(workspace.path().join("plan.md"), vec![b'a'; 131_073]).unwrap();
    let submit = plan_with_sid(&mut host, 2, "lato/plan/submit", &sid).await;
    assert!(
        submit.get("error").is_some(),
        "oversized draft must fail closed"
    );

    // At the limit it submits and approval records the content hash.
    std::fs::write(workspace.path().join("plan.md"), vec![b'a'; 131_072]).unwrap();
    let submit = plan_with_sid(&mut host, 3, "lato/plan/submit", &sid).await;
    assert_eq!(submit["result"]["phase"], "awaiting_approval");

    // A directory in place of the draft makes approval fail closed.
    std::fs::remove_file(workspace.path().join("plan.md")).unwrap();
    std::fs::create_dir(workspace.path().join("plan.md")).unwrap();
    let approve = plan_with_sid(&mut host, 4, "lato/plan/approve", &sid).await;
    assert!(
        approve.get("error").is_some(),
        "unreadable draft must fail closed"
    );
    let status = plan_with_sid(&mut host, 5, "lato/plan/status", &sid).await;
    assert_eq!(status["result"]["phase"], "awaiting_approval");
}
