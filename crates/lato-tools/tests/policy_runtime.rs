use async_trait::async_trait;
use lato_core::{
    PolicyDecision, PolicyMode, SandboxProfile, SessionId, SideEffect, Tool, ToolCallId,
    ToolCancellation, ToolCapability, ToolConcurrency, ToolContext, ToolDescriptor,
    ToolIdempotency, ToolLayer, ToolName, ToolOutput, ToolSource, TurnId,
};
use lato_policy::{ApprovalLedger, PolicyEngine};
use lato_tools::{PolicyScope, ToolRuntimeBuilder};
use semver::Version;
use serde_json::json;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

struct CountingTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::parse("builtin:write_file").unwrap(),
            version: Version::new(1, 0, 0),
            description: "count writes".into(),
            input_schema: json!({"type":"object"}),
            capabilities: vec![ToolCapability::FileWrite],
            side_effect: SideEffect::WorkspaceMutation,
            concurrency: ToolConcurrency::Serial,
            idempotency: ToolIdempotency::NonIdempotent,
            timeout_ms: 1_000,
            max_output_bytes: 1_024,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource {
                layer: ToolLayer::Builtin,
                id: "test.write".into(),
                replacement: None,
            },
        }
    }

    async fn invoke(
        &self,
        context: ToolContext,
        _arguments: serde_json::Value,
    ) -> Result<ToolOutput, lato_core::ToolError> {
        assert!(context.execution_grant.is_some());
        self.calls.fetch_add(1, Ordering::AcqRel);
        Ok(ToolOutput {
            content: "written".into(),
            metadata: json!({}),
            truncated: false,
            artifact_path: None,
        })
    }
}

fn runtime(root: &Path, calls: Arc<AtomicUsize>) -> lato_tools::ToolRuntime {
    let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
        Duration::from_secs(60),
    ))));
    let scope = PolicyScope {
        workspace_root: root.to_path_buf(),
        mode: PolicyMode::Ask,
        project_trusted: true,
        sandbox_profile: SandboxProfile::Workspace,
    };
    let mut builder = ToolRuntimeBuilder::new(policy, scope);
    builder.register(Arc::new(CountingTool { calls })).unwrap();
    builder.build().unwrap()
}

fn context(call_id: &str) -> ToolContext {
    ToolContext {
        session_id: SessionId::from("session-1"),
        turn_id: TurnId::from("turn-1"),
        call_id: ToolCallId::from(call_id),
        cancellation: CancellationToken::new(),
        execution_grant: None,
    }
}

#[tokio::test]
async fn approval_required_call_cannot_bypass_policy() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(root.path(), calls.clone());
    let prepared = runtime
        .prepare(
            context("call-1"),
            "write",
            json!({"contents":"x","path":"a"}),
        )
        .unwrap();
    assert!(matches!(
        runtime.decision(&prepared),
        PolicyDecision::RequireApproval(_)
    ));

    let error = runtime
        .execute_without_approval_for_test(prepared)
        .await
        .unwrap_err();
    assert_eq!(error.code, "policy.grant_missing");
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

#[test]
fn aliases_are_canonical_before_fingerprinting() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path(), Arc::new(AtomicUsize::new(0)));
    let arguments = json!({"contents":"x","path":"a"});

    let fingerprints = ["write", "write_file", "Lato:write"].map(|wire_name| {
        let prepared = runtime
            .prepare(context("call-1"), wire_name, arguments.clone())
            .unwrap();
        let PolicyDecision::RequireApproval(request) = runtime.decision(&prepared) else {
            panic!("write must require approval");
        };
        assert_eq!(request.request.tool_name.as_str(), "builtin:write_file");
        request.fingerprint.clone()
    });
    assert!(fingerprints.windows(2).all(|pair| pair[0] == pair[1]));
}

#[tokio::test]
async fn grant_is_consumed_before_tool_invocation() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(root.path(), calls.clone());
    let prepared = runtime
        .prepare(
            context("call-1"),
            "write_file",
            json!({"path":"a","contents":"x"}),
        )
        .unwrap();
    let PolicyDecision::RequireApproval(request) = runtime.decision(&prepared) else {
        panic!("write must require approval");
    };
    let grant = runtime.approve(request).unwrap();
    runtime.execute(prepared, grant.clone()).await.unwrap();
    assert_eq!(calls.load(Ordering::Acquire), 1);

    let replay = runtime
        .prepare(
            context("call-1"),
            "write_file",
            json!({"path":"a","contents":"x"}),
        )
        .unwrap();
    let error = runtime.execute(replay, grant).await.unwrap_err();
    assert_eq!(error.code, "policy.grant_consumed");
    assert_eq!(calls.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn invoke_only_executes_automatic_allow_decisions() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(root.path(), calls.clone());
    let error = runtime
        .invoke(
            context("call-1"),
            "write_file",
            json!({"path":"a","contents":"x"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "policy.approval_required");
    assert_eq!(calls.load(Ordering::Acquire), 0);
}
