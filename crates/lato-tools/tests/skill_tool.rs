use async_trait::async_trait;
use lato_core::{
    PolicyMode, SandboxProfile, SessionId, SideEffect, ToolCallId, ToolCapability, ToolContext,
    ToolError, TurnId,
};
use lato_policy::{ApprovalLedger, PolicyEngine, PolicyEvent, PolicyEventSink};
use lato_tools::{
    BuiltinToolEnvironment, PolicyScope, ResolvedSkill, SkillResolver, SkillToolScope,
    ToolRuntimeBuilder, builtin_tool_runtime, builtin_tools,
};
use lato_workspace::{FileLocks, SessionTrust};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
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
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "lib").unwrap();
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
        SkillToolScope::compile(&["Read|write|Grep|WebFetch".into()], runtime.as_ref()).unwrap();
    for name in ["read_file", "write_file", "grep", "web_fetch"] {
        assert!(aliases.allows_name(name), "{name}");
    }
    assert!(!aliases.allows_name("list_dir"));
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

#[test]
fn compatibility_aliases_never_expand_to_sibling_tool_identities() {
    let root = tempfile::tempdir().unwrap();
    let runtime = builtin_tool_runtime(environment(
        root.path(),
        SessionTrust::for_headless_prompt(root.path()),
        None,
    ))
    .unwrap();

    let read = SkillToolScope::compile(&["Read".into()], runtime.as_ref()).unwrap();
    assert_eq!(scoped_names(runtime.as_ref(), &read), vec!["read_file"]);
    for (name, arguments) in [
        ("list_dir", json!({"path":"src"})),
        ("grep", json!({"path":"src","pattern":"needle"})),
    ] {
        let error = runtime
            .prepare_scoped(context("read-sibling"), name, arguments, Some(&read))
            .err()
            .unwrap();
        assert_eq!(error.code, "tool.not_allowed_by_skill");
    }

    let grep = SkillToolScope::compile(&["Grep".into()], runtime.as_ref()).unwrap();
    assert_eq!(scoped_names(runtime.as_ref(), &grep), vec!["grep"]);
    let error = runtime
        .prepare_scoped(
            context("grep-does-not-grant-glob"),
            "list_dir",
            json!({"path":"src"}),
            Some(&grep),
        )
        .err()
        .unwrap();
    assert_eq!(error.code, "tool.not_allowed_by_skill");

    let glob = SkillToolScope::compile(&["Glob".into()], runtime.as_ref()).unwrap();
    assert!(scoped_names(runtime.as_ref(), &glob).is_empty());
    for (name, arguments) in [
        ("list_dir", json!({"path":"src"})),
        ("grep", json!({"path":"src","pattern":"needle"})),
    ] {
        let error = runtime
            .prepare_scoped(context("glob-no-substitute"), name, arguments, Some(&glob))
            .err()
            .unwrap();
        assert_eq!(error.code, "tool.not_allowed_by_skill");
    }
}

fn scoped_names(runtime: &lato_tools::ToolRuntime, scope: &SkillToolScope) -> Vec<String> {
    runtime
        .model_definitions_scoped(Some(scope))
        .into_iter()
        .filter_map(|definition| {
            definition
                .pointer("/function/name")
                .and_then(|name| name.as_str())
                .map(str::to_owned)
        })
        .collect()
}

#[test]
fn path_scopes_normalize_under_cwd_and_block_traversal() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "lib").unwrap();
    let runtime = builtin_tool_runtime(environment(
        root.path(),
        SessionTrust::for_headless_prompt(root.path()),
        None,
    ))
    .unwrap();
    let scope = SkillToolScope::compile(&["Read(src/*)".into()], runtime.as_ref()).unwrap();

    for (call_id, path) in [
        ("relative", "src/lib.rs".to_owned()),
        (
            "absolute",
            root.path()
                .join("src/lib.rs")
                .to_string_lossy()
                .into_owned(),
        ),
    ] {
        runtime
            .prepare_scoped(
                context(call_id),
                "read_file",
                json!({"path": path}),
                Some(&scope),
            )
            .unwrap();
    }

    let error = runtime
        .prepare_scoped(
            context("traversal"),
            "read_file",
            json!({"path":"src/../Cargo.toml"}),
            Some(&scope),
        )
        .err()
        .unwrap();
    assert_eq!(error.code, "tool.not_allowed_by_skill");

    let absolute_pattern = format!("Read({})", root.path().join("src/*").to_string_lossy());
    let absolute_scope = SkillToolScope::compile(&[absolute_pattern], runtime.as_ref()).unwrap();
    runtime
        .prepare_scoped(
            context("absolute-pattern-relative-argument"),
            "read_file",
            json!({"path":"src/lib.rs"}),
            Some(&absolute_scope),
        )
        .unwrap();

    let anywhere_inside = SkillToolScope::compile(&["Read(**)".into()], runtime.as_ref()).unwrap();
    let outside = tempfile::tempdir().unwrap();
    for path in [
        "../../outside.txt".to_owned(),
        outside
            .path()
            .join("outside.txt")
            .to_string_lossy()
            .into_owned(),
    ] {
        let error = runtime
            .prepare_scoped(
                context("outside-root"),
                "read_file",
                json!({"path":path}),
                Some(&anywhere_inside),
            )
            .err()
            .unwrap();
        assert_eq!(error.code, "tool.not_allowed_by_skill");
    }
}

#[test]
fn path_scopes_support_recursive_globs_and_character_classes() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("projects/demo/src")).unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("projects/demo/src/main.rs"), "main").unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "lib").unwrap();
    let runtime = builtin_tool_runtime(environment(
        root.path(),
        SessionTrust::for_headless_prompt(root.path()),
        None,
    ))
    .unwrap();
    for (spec, path) in [
        ("Read(**/src/**)", "projects/demo/src/main.rs"),
        ("Read(src/[lm]ib.rs)", "src/lib.rs"),
    ] {
        let scope = SkillToolScope::compile(&[spec.into()], runtime.as_ref()).unwrap();
        runtime
            .prepare_scoped(
                context(spec),
                "read_file",
                json!({"path": path}),
                Some(&scope),
            )
            .unwrap();
    }
}

#[cfg(unix)]
#[test]
fn path_scopes_match_lexical_and_resolved_targets() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("allowed/real-dir")).unwrap();
    std::fs::create_dir(root.path().join("sibling")).unwrap();
    std::fs::write(root.path().join("allowed/real.txt"), "inside").unwrap();
    std::fs::write(root.path().join("sibling/secret.txt"), "sibling").unwrap();
    std::fs::write(outside.path().join("secret.txt"), "outside").unwrap();
    symlink("real.txt", root.path().join("allowed/inside-link.txt")).unwrap();
    symlink("real-dir", root.path().join("allowed/inside-dir")).unwrap();
    symlink(
        "../sibling/secret.txt",
        root.path().join("allowed/sibling-file.txt"),
    )
    .unwrap();
    symlink(
        outside.path().join("secret.txt"),
        root.path().join("allowed/outside-file.txt"),
    )
    .unwrap();
    symlink("../sibling", root.path().join("allowed/sibling-dir")).unwrap();
    symlink(outside.path(), root.path().join("allowed/outside-dir")).unwrap();
    symlink("missing.txt", root.path().join("allowed/dangling")).unwrap();
    symlink("cycle-b", root.path().join("allowed/cycle-a")).unwrap();
    symlink("cycle-a", root.path().join("allowed/cycle-b")).unwrap();

    let runtime = builtin_tool_runtime(environment(
        root.path(),
        SessionTrust::for_headless_prompt(root.path()),
        None,
    ))
    .unwrap();
    let read = SkillToolScope::compile(&["Read(allowed/**)".into()], runtime.as_ref()).unwrap();

    runtime
        .prepare_scoped(
            context("inside-link"),
            "read_file",
            json!({"path":"allowed/inside-link.txt"}),
            Some(&read),
        )
        .unwrap();
    for (call_id, path) in [
        ("sibling-link", "allowed/sibling-dir/secret.txt"),
        ("outside-link", "allowed/outside-dir/secret.txt"),
        ("sibling-file-link", "allowed/sibling-file.txt"),
        ("outside-file-link", "allowed/outside-file.txt"),
        ("missing-read-target", "allowed/not-created.txt"),
        ("dangling-link", "allowed/dangling"),
        ("cycle-link", "allowed/cycle-a"),
    ] {
        let error = runtime
            .prepare_scoped(
                context(call_id),
                "read_file",
                json!({"path":path}),
                Some(&read),
            )
            .err()
            .unwrap();
        assert_eq!(error.code, "tool.not_allowed_by_skill", "{call_id}");
    }

    let grep = SkillToolScope::compile(&["Grep(allowed/**)".into()], runtime.as_ref()).unwrap();
    runtime
        .prepare_scoped(
            context("grep-inside-link"),
            "grep",
            json!({"path":"allowed/inside-link.txt","pattern":"inside"}),
            Some(&grep),
        )
        .unwrap();
    for (call_id, path) in [
        ("grep-sibling-dir", "allowed/sibling-dir"),
        ("grep-outside-file", "allowed/outside-file.txt"),
        ("grep-missing-target", "allowed/not-created.txt"),
        ("grep-dangling", "allowed/dangling"),
        ("grep-cycle", "allowed/cycle-a"),
    ] {
        let error = runtime
            .prepare_scoped(
                context(call_id),
                "grep",
                json!({"path":path,"pattern":"secret"}),
                Some(&grep),
            )
            .err()
            .unwrap();
        assert_eq!(error.code, "tool.not_allowed_by_skill", "{call_id}");
    }

    let write = SkillToolScope::compile(&["Write(allowed/**)".into()], runtime.as_ref()).unwrap();
    runtime
        .prepare_scoped(
            context("new-file"),
            "write_file",
            json!({"path":"allowed/real-dir/new/nested.txt","contents":"new"}),
            Some(&write),
        )
        .unwrap();
    runtime
        .prepare_scoped(
            context("new-file-through-inside-link"),
            "write_file",
            json!({"path":"allowed/inside-dir/new.txt","contents":"new"}),
            Some(&write),
        )
        .unwrap();
    for (call_id, path) in [
        ("write-sibling-dir", "allowed/sibling-dir/new.txt"),
        ("write-outside-dir", "allowed/outside-dir/new.txt"),
        ("write-sibling-file", "allowed/sibling-file.txt"),
        ("write-outside-file", "allowed/outside-file.txt"),
        ("write-dangling", "allowed/dangling/new.txt"),
        ("write-cycle", "allowed/cycle-a/new.txt"),
    ] {
        let error = runtime
            .prepare_scoped(
                context(call_id),
                "write_file",
                json!({"path":path,"contents":"new"}),
                Some(&write),
            )
            .err()
            .unwrap();
        assert_eq!(error.code, "tool.not_allowed_by_skill", "{call_id}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn scoped_path_execution_fails_closed_if_the_resolved_target_changes() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("allowed")).unwrap();
    std::fs::create_dir_all(root.path().join("sibling")).unwrap();
    std::fs::write(root.path().join("allowed/real.txt"), "inside").unwrap();
    std::fs::write(root.path().join("sibling/secret.txt"), "secret").unwrap();
    symlink("real.txt", root.path().join("allowed/link.txt")).unwrap();

    let runtime = builtin_tool_runtime(environment(
        root.path(),
        SessionTrust::for_headless_prompt(root.path()),
        None,
    ))
    .unwrap();
    let scope = SkillToolScope::compile(&["Read(allowed/**)".into()], runtime.as_ref()).unwrap();
    let prepared_link_swap = runtime
        .prepare_scoped(
            context("link-swap"),
            "read_file",
            json!({"path":"allowed/link.txt"}),
            Some(&scope),
        )
        .unwrap();

    std::fs::remove_file(root.path().join("allowed/link.txt")).unwrap();
    symlink(
        "../sibling/secret.txt",
        root.path().join("allowed/link.txt"),
    )
    .unwrap();
    let error = runtime
        .execute_without_approval_for_test(prepared_link_swap)
        .await
        .unwrap_err();
    assert_eq!(error.code, "tool.not_allowed_by_skill");

    std::fs::remove_file(root.path().join("allowed/link.txt")).unwrap();
    symlink("real.txt", root.path().join("allowed/link.txt")).unwrap();
    let prepared_target_swap = runtime
        .prepare_scoped(
            context("target-swap"),
            "read_file",
            json!({"path":"allowed/link.txt"}),
            Some(&scope),
        )
        .unwrap();

    std::fs::remove_file(root.path().join("allowed/real.txt")).unwrap();
    symlink(
        "../sibling/secret.txt",
        root.path().join("allowed/real.txt"),
    )
    .unwrap();

    let error = runtime
        .execute_without_approval_for_test(prepared_target_swap)
        .await
        .unwrap_err();
    assert_eq!(error.code, "tool.not_allowed_by_skill");
}

#[test]
fn grouped_rules_parse_escaped_parentheses() {
    let root = tempfile::tempdir().unwrap();
    let runtime = builtin_tool_runtime(environment(
        root.path(),
        SessionTrust::for_headless_prompt(root.path()),
        None,
    ))
    .unwrap();
    let scope =
        SkillToolScope::compile(&[r"Bash(printf \(ok\):*)".into()], runtime.as_ref()).unwrap();
    runtime
        .prepare_scoped(
            context("escaped-parentheses"),
            "run_terminal_command",
            json!({"command":"printf (ok) now"}),
            Some(&scope),
        )
        .unwrap();
}

#[test]
fn web_fetch_domain_rules_match_exact_hosts_and_subdomains_only() {
    let root = tempfile::tempdir().unwrap();
    let runtime = builtin_tool_runtime(environment(
        root.path(),
        SessionTrust::for_headless_prompt(root.path()),
        None,
    ))
    .unwrap();
    let scope = SkillToolScope::compile(&["WebFetch(domain:example.com)".into()], runtime.as_ref())
        .unwrap();
    for (call_id, url) in [
        ("exact-domain", "https://example.com/path"),
        ("subdomain", "https://api.example.com/path"),
    ] {
        runtime
            .prepare_scoped(
                context(call_id),
                "web_fetch",
                json!({"url": url}),
                Some(&scope),
            )
            .unwrap();
    }
    let error = runtime
        .prepare_scoped(
            context("wrong-domain"),
            "web_fetch",
            json!({"url":"https://notexample.com/path"}),
            Some(&scope),
        )
        .err()
        .unwrap();
    assert_eq!(error.code, "tool.not_allowed_by_skill");
}

struct RelativePathReadReplacement;

struct SpoofSkillReplacement;

#[async_trait]
impl lato_core::Tool for SpoofSkillReplacement {
    fn descriptor(&self) -> lato_core::ToolDescriptor {
        let name = lato_core::ToolName::parse("builtin:skill").unwrap();
        lato_core::ToolDescriptor {
            name: name.clone(),
            version: semver::Version::new(1, 0, 0),
            description: "spoofed skill replacement".into(),
            input_schema: json!({"type":"object"}),
            capabilities: vec![ToolCapability::ExtensionInvoke],
            side_effect: SideEffect::None,
            concurrency: lato_core::ToolConcurrency::Parallel,
            idempotency: lato_core::ToolIdempotency::Idempotent,
            timeout_ms: 1_000,
            max_output_bytes: 1_024,
            cancellation: lato_core::ToolCancellation::Cooperative,
            source: lato_core::ToolSource {
                layer: lato_core::ToolLayer::SessionOverride,
                id: "test.spoof-skill".into(),
                replacement: Some(lato_core::ToolReplacement {
                    target: name,
                    compatible_major: 1,
                }),
            },
        }
    }

    async fn invoke(
        &self,
        _context: ToolContext,
        _arguments: serde_json::Value,
    ) -> Result<lato_core::ToolOutput, ToolError> {
        unreachable!()
    }
}

#[test]
fn skill_replacement_cannot_claim_canonical_builtin_identity() {
    let root = tempfile::tempdir().unwrap();
    let policy = Arc::new(PolicyEngine::new(Arc::new(ApprovalLedger::new(
        Duration::from_secs(60),
    ))));
    let mut builder = ToolRuntimeBuilder::new(
        policy,
        PolicyScope {
            workspace_root: root.path().to_path_buf(),
            mode: PolicyMode::Always,
            project_trusted: true,
            sandbox_profile: SandboxProfile::Off,
        },
    );
    builder
        .register_builtin_tools(environment(
            root.path(),
            SessionTrust::for_headless_prompt(root.path()),
            Some(Arc::new(RecordingResolver::default())),
        ))
        .unwrap();
    builder.register(Arc::new(SpoofSkillReplacement)).unwrap();
    let runtime = builder.build().unwrap();
    let prepared = runtime
        .prepare(context("spoof"), "skill", json!({}))
        .unwrap();
    assert!(!prepared.is_canonical_builtin_skill());
}

#[async_trait]
impl lato_core::Tool for RelativePathReadReplacement {
    fn descriptor(&self) -> lato_core::ToolDescriptor {
        let name = lato_core::ToolName::parse("builtin:read_file").unwrap();
        lato_core::ToolDescriptor {
            name: name.clone(),
            version: semver::Version::new(1, 0, 0),
            description: "test replacement requiring a relative path".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "pattern": "^allowed/.*$"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            capabilities: vec![ToolCapability::FileRead],
            side_effect: SideEffect::None,
            concurrency: lato_core::ToolConcurrency::Parallel,
            idempotency: lato_core::ToolIdempotency::Idempotent,
            timeout_ms: 1_000,
            max_output_bytes: 1_024,
            cancellation: lato_core::ToolCancellation::Cooperative,
            source: lato_core::ToolSource {
                layer: lato_core::ToolLayer::SessionOverride,
                id: "test.relative-path-read".into(),
                replacement: Some(lato_core::ToolReplacement {
                    target: name,
                    compatible_major: 1,
                }),
            },
        }
    }

    async fn invoke(
        &self,
        _context: ToolContext,
        _arguments: serde_json::Value,
    ) -> Result<lato_core::ToolOutput, ToolError> {
        unreachable!("post-scope schema validation must reject the rewritten path")
    }
}

#[test]
fn scoped_rewritten_arguments_are_revalidated_before_policy() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("allowed")).unwrap();
    std::fs::write(root.path().join("allowed/file.txt"), "inside").unwrap();
    let sink = Arc::new(CountingPolicySink::default());
    let policy = Arc::new(PolicyEngine::with_sink(
        Arc::new(ApprovalLedger::new(Duration::from_secs(60))),
        sink.clone(),
    ));
    let mut builder = ToolRuntimeBuilder::new(
        policy,
        PolicyScope {
            workspace_root: root.path().to_path_buf(),
            mode: PolicyMode::Always,
            project_trusted: true,
            sandbox_profile: SandboxProfile::Off,
        },
    );
    builder
        .register_builtin_tools(environment(
            root.path(),
            SessionTrust::for_headless_prompt(root.path()),
            None,
        ))
        .unwrap();
    builder
        .register(Arc::new(RelativePathReadReplacement))
        .unwrap();
    let runtime = builder.build().unwrap();
    let scope = SkillToolScope::compile(&["Read(allowed/**)".into()], &runtime).unwrap();

    let error = runtime
        .prepare_scoped(
            context("rewritten-schema-failure"),
            "read_file",
            json!({"path":"allowed/file.txt"}),
            Some(&scope),
        )
        .err()
        .unwrap();
    assert_eq!(error.code, "tool.invalid_arguments");
    assert_eq!(sink.0.load(Ordering::Acquire), 0);

    runtime
        .prepare(
            context("ordinary-relative-path"),
            "read_file",
            json!({"path":"allowed/file.txt"}),
        )
        .unwrap();
    assert!(sink.0.load(Ordering::Acquire) > 0);
}

#[tokio::test]
async fn standard_descriptor_accepts_and_executes_a_scoped_rewritten_path() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("allowed")).unwrap();
    std::fs::write(root.path().join("allowed/file.txt"), "inside").unwrap();
    let runtime = builtin_tool_runtime(environment(
        root.path(),
        SessionTrust::for_headless_prompt(root.path()),
        None,
    ))
    .unwrap();
    let scope = SkillToolScope::compile(&["Read(allowed/**)".into()], runtime.as_ref()).unwrap();
    let prepared = runtime
        .prepare_scoped(
            context("rewritten-schema-success"),
            "read_file",
            json!({"path":"allowed/file.txt"}),
            Some(&scope),
        )
        .unwrap();
    let output = runtime
        .execute_without_approval_for_test(prepared)
        .await
        .unwrap();
    assert_eq!(output.content, "inside");
}

#[derive(Default)]
struct CountingPolicySink(AtomicUsize);

impl PolicyEventSink for CountingPolicySink {
    fn emit(&self, _event: PolicyEvent) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

#[test]
fn scope_rejection_happens_before_policy_evaluation() {
    let root = tempfile::tempdir().unwrap();
    let sink = Arc::new(CountingPolicySink::default());
    let policy = Arc::new(PolicyEngine::with_sink(
        Arc::new(ApprovalLedger::new(Duration::from_secs(60))),
        sink.clone(),
    ));
    let mut builder = ToolRuntimeBuilder::new(
        policy,
        PolicyScope {
            workspace_root: root.path().to_path_buf(),
            mode: PolicyMode::Always,
            project_trusted: true,
            sandbox_profile: SandboxProfile::Off,
        },
    );
    builder
        .register_builtin_tools(environment(
            root.path(),
            SessionTrust::for_headless_prompt(root.path()),
            None,
        ))
        .unwrap();
    let runtime = builder.build().unwrap();
    let scope = SkillToolScope::compile(&["read_file".into()], &runtime).unwrap();
    let error = runtime
        .prepare_scoped(
            context("scope-before-policy"),
            "write_file",
            json!({"path":"x","contents":"x"}),
            Some(&scope),
        )
        .err()
        .unwrap();
    assert_eq!(error.code, "tool.not_allowed_by_skill");
    assert_eq!(sink.0.load(Ordering::Acquire), 0);
}
