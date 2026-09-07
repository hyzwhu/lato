use async_trait::async_trait;
use lato_core::{
    SessionId, SideEffect, ToolCallId, ToolCapability, ToolContext, ToolError, TurnId,
};
use lato_tools::{
    BuiltinToolEnvironment, ResolvedSkill, SkillResolver, SkillToolScope, builtin_tool_runtime,
    builtin_tools,
};
use lato_workspace::{FileLocks, SessionTrust};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct RecordingResolver {
    calls: AtomicUsize,
}

#[async_trait]
impl SkillResolver for RecordingResolver {
    async fn invoke(
        &self,
        _context: &ToolContext,
        skill: &str,
        args: Option<&str>,
    ) -> Result<ResolvedSkill, ToolError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        assert_eq!(skill, "demo:inspect");
        assert_eq!(args, Some("src/runtime.rs"));
        Ok(ResolvedSkill {
            qualified_name: skill.to_owned(),
            message: "<skill>inspect</skill>".into(),
            allowed_tool_specs: Some(vec!["read_file".into()]),
            body_hash: "abc123".into(),
        })
    }
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

fn environment(
    root: &std::path::Path,
    trust: SessionTrust,
    resolver: Option<Arc<dyn SkillResolver>>,
) -> BuiltinToolEnvironment {
    BuiltinToolEnvironment {
        cwd: root.to_path_buf(),
        locks: Arc::new(FileLocks::new()),
        trust,
        skill_resolver: resolver,
    }
}

#[test]
fn skill_descriptor_is_registered_only_with_a_resolver() {
    let root = tempfile::tempdir().unwrap();
    let without = builtin_tools(environment(
        root.path(),
        SessionTrust::for_headless_prompt(root.path()),
        None,
    ))
    .unwrap();
    assert!(
        without
            .iter()
            .all(|tool| tool.descriptor().name.local_name() != "skill")
    );

    let resolver: Arc<dyn SkillResolver> = Arc::new(RecordingResolver::default());
    let with = builtin_tools(environment(
        root.path(),
        SessionTrust::for_headless_prompt(root.path()),
        Some(resolver),
    ))
    .unwrap();
    let descriptor = with
        .iter()
        .map(|tool| tool.descriptor())
        .find(|descriptor| descriptor.name.local_name() == "skill")
        .unwrap();
    assert_eq!(
        descriptor.capabilities,
        vec![ToolCapability::ExtensionInvoke]
    );
    assert_eq!(descriptor.side_effect, SideEffect::None);
    assert_eq!(
        descriptor.input_schema,
        json!({
            "type": "object",
            "properties": {
                "skill": {"type": "string"},
                "args": {"type": "string"}
            },
            "required": ["skill"],
            "additionalProperties": false
        })
    );
}

#[tokio::test]
async fn direct_skill_tool_invocation_requires_a_policy_grant() {
    let root = tempfile::tempdir().unwrap();
    let resolver = Arc::new(RecordingResolver::default());
    let tools = builtin_tools(environment(
        root.path(),
        SessionTrust::for_headless_prompt(root.path()),
        Some(resolver.clone()),
    ))
    .unwrap();
    let skill = tools
        .into_iter()
        .find(|tool| tool.descriptor().name.local_name() == "skill")
        .unwrap();
    let error = skill
        .invoke(
            context("direct-call"),
            json!({"skill":"demo:inspect","args":"src/runtime.rs"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "policy.grant_missing");
    assert_eq!(resolver.calls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn policy_denial_prevents_resolver_invocation() {
    let root = tempfile::tempdir().unwrap();
    let resolver = Arc::new(RecordingResolver::default());
    let runtime = builtin_tool_runtime(environment(
        root.path(),
        SessionTrust::for_interactive(root.path(), false),
        Some(resolver.clone()),
    ))
    .unwrap();

    let error = runtime
        .invoke(
            context("call-denied"),
            "skill",
            json!({"skill":"demo:inspect","args":"src/runtime.rs"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "policy.untrusted_extension");
    assert_eq!(resolver.calls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn skill_invocation_preserves_bounded_resolver_metadata() {
    let root = tempfile::tempdir().unwrap();
    let resolver = Arc::new(RecordingResolver::default());
    let runtime = builtin_tool_runtime(environment(
        root.path(),
        SessionTrust::for_headless_prompt(root.path()),
        Some(resolver.clone()),
    ))
    .unwrap();

    let output = runtime
        .invoke(
            context("call-allowed"),
            "skill",
            json!({"args":"src/runtime.rs","skill":"demo:inspect"}),
        )
        .await
        .unwrap();
    assert_eq!(output.content, "<skill>inspect</skill>");
    assert_eq!(output.metadata["kind"], "skill_invocation");
    assert_eq!(output.metadata["qualifiedName"], "demo:inspect");
    assert_eq!(output.metadata["allowedToolSpecs"], json!(["read_file"]));
    assert_eq!(output.metadata["bodyHash"], "abc123");
    assert_eq!(output.metadata.as_object().unwrap().len(), 4);
    assert_eq!(resolver.calls.load(Ordering::Acquire), 1);
}

struct OversizedMetadataResolver;

#[async_trait]
impl SkillResolver for OversizedMetadataResolver {
    async fn invoke(
        &self,
        _context: &ToolContext,
        _skill: &str,
        _args: Option<&str>,
    ) -> Result<ResolvedSkill, ToolError> {
        Ok(ResolvedSkill {
            qualified_name: "x".repeat(130),
            message: "body".into(),
            allowed_tool_specs: None,
            body_hash: "abc123".into(),
        })
    }
}

#[tokio::test]
async fn skill_invocation_rejects_oversized_metadata() {
    let root = tempfile::tempdir().unwrap();
    let runtime = builtin_tool_runtime(environment(
        root.path(),
        SessionTrust::for_headless_prompt(root.path()),
        Some(Arc::new(OversizedMetadataResolver)),
    ))
    .unwrap();
    let error = runtime
        .invoke(
            context("call-oversized"),
            "skill",
            json!({"skill":"demo:inspect"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "skill.invalid_metadata");
}

#[test]
fn scoped_runtime_filters_definitions_and_checks_grouped_arguments() {
    let root = tempfile::tempdir().unwrap();
    let runtime = builtin_tool_runtime(environment(
        root.path(),
        SessionTrust::for_headless_prompt(root.path()),
        None,
    ))
    .unwrap();
    let scope =
        SkillToolScope::compile(&["read_file|Bash(git diff:*)".into()], runtime.as_ref()).unwrap();
    let names = runtime
        .model_definitions_scoped(Some(&scope))
        .into_iter()
        .filter_map(|definition| {
            definition
                .pointer("/function/name")
                .and_then(|name| name.as_str())
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["read_file", "run_terminal_command"]);

    runtime
        .prepare_scoped(
            context("call-read"),
            "read_file",
            json!({"path":"src/lib.rs"}),
            Some(&scope),
        )
        .unwrap();
    runtime
        .prepare_scoped(
            context("call-bash"),
            "run_terminal_command",
            json!({"command":"   git diff --stat"}),
            Some(&scope),
        )
        .unwrap();

    for (name, arguments) in [
        ("write_file", json!({"path":"x","contents":"x"})),
        ("run_terminal_command", json!({"command":"cargo test"})),
    ] {
        let error = runtime
            .prepare_scoped(context("call-denied"), name, arguments, Some(&scope))
            .err()
            .unwrap();
        assert_eq!(error.code, "tool.not_allowed_by_skill");
    }
}

#[test]
fn scopes_support_compatibility_aliases_wildcards_and_canonical_validation() {
    let root = tempfile::tempdir().unwrap();
    let runtime = builtin_tool_runtime(environment(
        root.path(),
        SessionTrust::for_headless_prompt(root.path()),
        None,
    ))
    .unwrap();

    let aliases =
        SkillToolScope::compile(&["Read|write|Grep|Glob|WebFetch".into()], runtime.as_ref())
            .unwrap();
    for name in ["read_file", "write_file", "grep", "list_dir", "web_fetch"] {
        assert!(aliases.allows_name(name), "{name}");
    }
    assert!(!aliases.allows_name("run_terminal_command"));

    let wildcard = SkillToolScope::compile(&["*".into()], runtime.as_ref()).unwrap();
    assert_eq!(
        runtime.model_definitions_scoped(Some(&wildcard)),
        runtime.model_definitions()
    );
    let empty = SkillToolScope::compile(&[], runtime.as_ref()).unwrap();
    assert_eq!(
        runtime.model_definitions_scoped(Some(&empty)),
        runtime.model_definitions()
    );

    let validated = runtime
        .resolve_and_validate("Lato:write", json!({"contents":"x","path":"a"}))
        .unwrap();
    assert_eq!(validated.wire_name, "Lato:write");
    assert_eq!(validated.canonical_name.as_str(), "builtin:write_file");
    assert_eq!(validated.arguments, json!({"contents":"x","path":"a"}));

    let error = runtime
        .resolve_and_validate("read_file", json!({}))
        .unwrap_err();
    assert_eq!(error.code, "tool.invalid_arguments");
}
