use lato_core::{SessionId, ToolCallId, ToolContext, TurnId};
use lato_tools::{BuiltinToolEnvironment, builtin_tools, v1_tool_definitions};
use lato_workspace::{FileLocks, SessionTrust};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn context(cancellation: CancellationToken) -> ToolContext {
    ToolContext {
        session_id: SessionId::from("session-1"),
        turn_id: TurnId::from("turn-1"),
        call_id: ToolCallId::from("call-1"),
        cancellation,
    }
}

#[test]
fn builtin_adapters_match_the_v1_model_definition_set() {
    let root = tempfile::tempdir().unwrap();
    let tools = builtin_tools(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(root.path()),
    })
    .unwrap();
    let mut actual = tools
        .iter()
        .map(|tool| tool.descriptor().name.local_name().to_owned())
        .collect::<Vec<_>>();
    let mut expected = v1_tool_definitions()
        .as_array()
        .unwrap()
        .iter()
        .map(|definition| {
            definition
                .pointer("/function/name")
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 9);
}

#[tokio::test]
async fn cancelled_adapter_does_not_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let tools = builtin_tools(BuiltinToolEnvironment {
        cwd: root.path().to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust: SessionTrust::for_headless_prompt(root.path()),
    })
    .unwrap();
    let read = tools
        .into_iter()
        .find(|tool| tool.descriptor().name.as_str() == "builtin:read_file")
        .unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = read
        .invoke(
            context(cancellation),
            serde_json::json!({"path": "missing"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "tool.cancelled");
}
