use std::{collections::VecDeque, fs, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use lato_agent::{
    AcpHost, ChildSessionConfig, HistoryItem, RuntimePromptOutcome, RuntimeSession, SessionActor,
    SessionSkillHandle,
};
use lato_ai::{ModelStream, StreamPiece, adapt_model_endpoint};
use lato_core::{
    ExtensionAuditRecord, JournalRecord, JournalReplay, SessionId, SessionStore, ToolCallId,
    ToolCapability, ToolContext, TurnId,
};
use lato_extensions::{
    CapabilityCeiling, DiscoveryConfig, PluginConfig, PluginSnapshot, build_snapshot,
    discover_plugins,
    skills::{SkillCatalog, discover_skills},
};
use lato_protocol::JsonRpcReq;
use lato_store::MemoryEventStore;
use lato_tools::{
    BuiltinToolEnvironment, SkillResolver, SkillToolScope, builtin_tool_runtime,
    builtin_tool_runtime_for_capabilities,
};
use lato_workspace::{FileLocks, SessionTrust};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

struct PluginFixture {
    _temp: tempfile::TempDir,
    workspace: PathBuf,
    home: PathBuf,
    plugin: PathBuf,
}

impl PluginFixture {
    fn new(name: &str, description: &str, allowed_tools: &[&str]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let home = temp.path().join("home");
        let plugin = temp.path().join(name);
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(home.join("plugins")).unwrap();
        fs::create_dir_all(plugin.join("skills/inspect")).unwrap();
        fs::write(
            plugin.join("plugin.json"),
            format!(r#"{{"name":"{name}","skills":"skills"}}"#),
        )
        .unwrap();
        let tools = allowed_tools
            .iter()
            .map(|tool| format!("  - {tool}\n"))
            .collect::<String>();
        fs::write(
            plugin.join("skills/inspect/SKILL.md"),
            format!(
                "---\nname: inspect\ndescription: {description}\nallowed-tools:\n{tools}---\nInspect $ARGUMENTS."
            ),
        )
        .unwrap();
        Self {
            _temp: temp,
            workspace,
            home,
            plugin,
        }
    }

    fn snapshot(&self, generation: u64, trusted: bool, enabled: bool) -> Arc<PluginSnapshot> {
        let discovery = discover_plugins(&DiscoveryConfig {
            cwd: self.workspace.clone(),
            lato_home: self.home.clone(),
            cli_plugin_dirs: vec![self.plugin.clone()],
            project_trusted: trusted,
        });
        let config = if enabled {
            PluginConfig::default()
        } else {
            PluginConfig {
                enabled: Vec::new(),
                disabled: vec![
                    self.plugin
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                ],
            }
        };
        build_snapshot(generation, discovery, &config).unwrap()
    }

    fn untrusted_project_snapshot(&self, generation: u64) -> Arc<PluginSnapshot> {
        let project_plugin = self.workspace.join(".lato/plugins/demo");
        fs::create_dir_all(project_plugin.join("skills/inspect")).unwrap();
        fs::copy(
            self.plugin.join("plugin.json"),
            project_plugin.join("plugin.json"),
        )
        .unwrap();
        fs::copy(
            self.plugin.join("skills/inspect/SKILL.md"),
            project_plugin.join("skills/inspect/SKILL.md"),
        )
        .unwrap();
        build_snapshot(
            generation,
            discover_plugins(&DiscoveryConfig {
                cwd: self.workspace.clone(),
                lato_home: self.home.clone(),
                cli_plugin_dirs: Vec::new(),
                project_trusted: false,
            }),
            &PluginConfig::default(),
        )
        .unwrap()
    }
}

#[derive(Default)]
struct RecordingStream {
    rounds: Mutex<VecDeque<Vec<StreamPiece>>>,
    contexts: Mutex<Vec<serde_json::Value>>,
}

impl RecordingStream {
    fn scripted(rounds: Vec<Vec<StreamPiece>>) -> Self {
        Self {
            rounds: Mutex::new(rounds.into()),
            contexts: Mutex::new(Vec::new()),
        }
    }

    async fn contexts(&self) -> Vec<serde_json::Value> {
        self.contexts.lock().await.clone()
    }
}

#[async_trait]
impl ModelStream for RecordingStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), lato_core::ModelError> {
        self.contexts.lock().await.push(context);
        let pieces = self
            .rounds
            .lock()
            .await
            .pop_front()
            .unwrap_or_else(|| vec![StreamPiece::Text("done".into())]);
        for piece in pieces {
            tx.send(piece)
                .await
                .map_err(|_| lato_core::ModelError::cancelled())?;
        }
        Ok(())
    }
}

fn tool_names(context: &serde_json::Value) -> Vec<&str> {
    context["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| {
            tool.pointer("/function/name")
                .and_then(|name| name.as_str())
        })
        .collect()
}

fn count(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

fn tool_context() -> ToolContext {
    ToolContext {
        session_id: SessionId::from("skill-session"),
        turn_id: TurnId::from("skill-turn"),
        call_id: ToolCallId::from("skill-call"),
        cancellation: CancellationToken::new(),
        execution_grant: None,
    }
}

fn runtime_session(fixture: &PluginFixture, stream: Arc<dyn ModelStream>) -> RuntimeSession {
    let (updates, _updates_rx) = mpsc::unbounded_channel();
    RuntimeSession::new(
        "skills-runtime".into(),
        stream,
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(&fixture.workspace),
        fixture.workspace.clone(),
        updates,
        None,
    )
}

fn invocation_script() -> Vec<Vec<StreamPiece>> {
    vec![
        vec![StreamPiece::ToolCall {
            id: "skill-call".into(),
            name: "skill".into(),
            arguments: serde_json::json!({"skill":"demo:inspect"}),
        }],
        vec![StreamPiece::Text("done".into())],
    ]
}

fn asserts_bound_skill_context(contexts: &[serde_json::Value]) {
    assert!(
        contexts[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("demo:inspect")
    );
    assert!(tool_names(&contexts[0]).contains(&"skill"));
    assert!(
        contexts[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message.get("role").and_then(serde_json::Value::as_str) == Some("tool")
                    && message
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|content| content.contains("<skill name=\"demo:inspect\""))
            })
    );
}

#[tokio::test]
async fn session_skill_handle_maps_catalog_errors_to_stable_codes() {
    let handle = SessionSkillHandle::default();
    let error = handle
        .invoke(&tool_context(), "missing", None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "skill.not_found");
    assert!(error.message.contains("missing"));
}

#[tokio::test]
async fn provider_listing_is_ephemeral_and_appended_once_to_first_system_message() {
    let fixture = PluginFixture::new("demo", "Review safely.", &["read_file"]);
    let stream = Arc::new(RecordingStream::scripted(vec![vec![StreamPiece::Text(
        "done".into(),
    )]]));
    let session = runtime_session(&fixture, stream.clone());
    session
        .stage_plugin_snapshot(fixture.snapshot(1, true, true))
        .await
        .unwrap();

    assert_eq!(
        session.prompt("inspect".into()).await.unwrap(),
        RuntimePromptOutcome::Complete {
            text: "done".into()
        }
    );
    let contexts = stream.contexts().await;
    let messages = contexts[0]["messages"].as_array().unwrap();
    let system = messages[0]["content"].as_str().unwrap();
    assert_eq!(count(system, "<available_skills>"), 1);
    assert!(system.contains("demo:inspect"));
    let history = session.history_snapshot().await;
    assert!(matches!(&history[0], HistoryItem::System(text) if !text.contains("available_skills")));
    assert!(
        history
            .iter()
            .all(|item| !format!("{item:?}").contains("available_skills"))
    );
}

#[tokio::test]
async fn model_definitions_include_skill_only_when_resolver_is_installed() {
    let fixture = PluginFixture::new("demo", "Review safely.", &["read_file"]);
    let stream = Arc::new(RecordingStream::scripted(vec![vec![StreamPiece::Text(
        "done".into(),
    )]]));
    let mut with_resolver = SessionActor::new(
        stream.clone(),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(&fixture.workspace),
        fixture.workspace.clone(),
    );
    with_resolver
        .prompt(lato_agent::PromptKind::Start, "hello".into())
        .await
        .unwrap();
    assert!(tool_names(&stream.contexts().await[0]).contains(&"skill"));

    let without_stream = Arc::new(RecordingStream::scripted(vec![vec![StreamPiece::Text(
        "done".into(),
    )]]));
    let locks = Arc::new(FileLocks::new());
    let trust = SessionTrust::for_headless_prompt(&fixture.workspace);
    let runtime = builtin_tool_runtime(BuiltinToolEnvironment {
        cwd: fixture.workspace.clone(),
        locks: locks.clone(),
        trust: trust.clone(),
        skill_resolver: None,
    })
    .unwrap();
    let mut without_resolver = SessionActor::new_with_tool_runtime(
        without_stream.clone(),
        locks,
        trust,
        fixture.workspace.clone(),
        runtime,
    );
    without_resolver
        .prompt(lato_agent::PromptKind::Start, "hello".into())
        .await
        .unwrap();
    assert!(!tool_names(&without_stream.contexts().await[0]).contains(&"skill"));
}

#[tokio::test]
async fn custom_runtime_without_matching_resolver_injects_neither_listing_nor_skill_tool() {
    let fixture = PluginFixture::new("demo", "Review safely.", &["read_file"]);
    let stream = Arc::new(RecordingStream::scripted(invocation_script()));
    let locks = Arc::new(FileLocks::new());
    let trust = SessionTrust::for_headless_prompt(&fixture.workspace);
    let runtime = builtin_tool_runtime(BuiltinToolEnvironment {
        cwd: fixture.workspace.clone(),
        locks: locks.clone(),
        trust: trust.clone(),
        // A caller-supplied resolver that is not paired with the session must
        // be inert through the compatibility constructor.
        skill_resolver: Some(Arc::new(SessionSkillHandle::default())),
    })
    .unwrap();
    let endpoint =
        adapt_model_endpoint("fixture", "custom", Default::default(), stream.clone()).unwrap();
    let (updates, _) = mpsc::unbounded_channel();
    let session = RuntimeSession::new_with_endpoint_and_tool_runtime(
        "custom-no-resolver".into(),
        endpoint,
        locks,
        trust,
        fixture.workspace.clone(),
        updates,
        None,
        runtime,
    );
    session
        .stage_plugin_snapshot(fixture.snapshot(1, true, true))
        .await
        .unwrap();
    session.prompt("hello".into()).await.unwrap();
    let contexts = stream.contexts().await;
    assert!(
        !contexts[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("available_skills")
    );
    assert!(!tool_names(&contexts[0]).contains(&"skill"));
    assert!(session.history_snapshot().await.iter().any(|item| {
        matches!(item, HistoryItem::ToolResult { output, .. } if output.contains("ERROR [skill.resolver_unbound]"))
    }));
}

#[tokio::test]
async fn production_host_resume_and_child_bind_the_same_resolver_handle() {
    let fixture = PluginFixture::new("demo", "Review safely.", &["read_file"]);

    let host_stream = Arc::new(RecordingStream::scripted(invocation_script()));
    let (updates, _) = mpsc::unbounded_channel();
    let mut host = AcpHost::new_with_home_and_plugin_dirs(
        fixture.workspace.clone(),
        SessionTrust::for_headless_prompt(&fixture.workspace),
        updates,
        host_stream.clone(),
        fixture.home.clone(),
        vec![fixture.plugin.clone()],
    );
    let new = host
        .handle(JsonRpcReq {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(1)),
            method: "session/new".into(),
            params: Some(serde_json::json!({})),
        })
        .await
        .unwrap();
    let sid = new["result"]["sessionId"].as_str().unwrap();
    let response = host
        .handle(JsonRpcReq {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(2)),
            method: "session/prompt".into(),
            params: Some(serde_json::json!({"sessionId":sid,"text":"inspect"})),
        })
        .await
        .unwrap();
    assert_eq!(response["result"]["status"], "complete");
    asserts_bound_skill_context(&host_stream.contexts().await);

    let resume_stream = Arc::new(RecordingStream::scripted(invocation_script()));
    let resume_id = SessionId::from("resume-skills");
    let replay = JournalReplay::empty(resume_id.clone());
    let store: Arc<dyn SessionStore> = Arc::new(MemoryEventStore::new());
    let (updates, _) = mpsc::unbounded_channel();
    let resumed = RuntimeSession::new_with_store(
        resume_id.to_string(),
        resume_stream.clone(),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(&fixture.workspace),
        fixture.workspace.clone(),
        updates,
        None,
        store,
        replay,
    )
    .await
    .unwrap();
    resumed
        .stage_plugin_snapshot(fixture.snapshot(2, true, true))
        .await
        .unwrap();
    resumed.prompt("inspect".into()).await.unwrap();
    asserts_bound_skill_context(&resume_stream.contexts().await);

    let child_stream = Arc::new(RecordingStream::scripted(invocation_script()));
    let locks = Arc::new(FileLocks::new());
    let trust = SessionTrust::for_headless_prompt(&fixture.workspace);
    let child_runtime = lato_agent::SkillRuntimeBinding::builtin_for_capabilities(
        fixture.workspace.clone(),
        locks.clone(),
        trust.clone(),
        Some(&[ToolCapability::FileRead, ToolCapability::ExtensionInvoke]),
    )
    .unwrap();
    let (updates, _) = mpsc::unbounded_channel();
    let child = RuntimeSession::new_child(ChildSessionConfig {
        session_id: "child-skills".into(),
        stream: child_stream.clone(),
        locks,
        trust,
        cwd: fixture.workspace.clone(),
        updates,
        approval: None,
        tool_runtime: lato_agent::ChildToolRuntime::from(child_runtime),
        initial_history: vec![HistoryItem::System("child".into())],
        plugin_snapshot: fixture.snapshot(3, true, true),
    })
    .await
    .unwrap();
    child.prompt("inspect".into()).await.unwrap();
    asserts_bound_skill_context(&child_stream.contexts().await);
}

#[tokio::test]
async fn skill_invocation_scopes_next_native_round_definitions_and_execution_then_clears() {
    let fixture = PluginFixture::new("demo", "Review safely.", &["read_file"]);
    fs::write(fixture.workspace.join("allowed.txt"), "ok").unwrap();
    let stream = Arc::new(RecordingStream::scripted(vec![
        vec![StreamPiece::ToolCall {
            id: "skill-call".into(),
            name: "skill".into(),
            arguments: serde_json::json!({"skill":"demo:inspect", "args":"allowed.txt"}),
        }],
        vec![StreamPiece::ToolCall {
            id: "denied-call".into(),
            name: "list_dir".into(),
            arguments: serde_json::json!({"path":"."}),
        }],
        vec![StreamPiece::Text("done".into())],
    ]));
    let session = runtime_session(&fixture, stream.clone());
    session
        .stage_plugin_snapshot(fixture.snapshot(1, true, true))
        .await
        .unwrap();

    session.prompt("inspect".into()).await.unwrap();
    let contexts = stream.contexts().await;
    assert!(tool_names(&contexts[0]).contains(&"skill"));
    assert_eq!(tool_names(&contexts[1]), vec!["read_file"]);
    assert!(tool_names(&contexts[2]).contains(&"list_dir"));
    let history = session.history_snapshot().await;
    assert!(history.iter().any(|item| matches!(item, HistoryItem::ToolResult { output, .. } if output.contains("<skill name=\"demo:inspect\"") && output.contains("Inspect allowed.txt."))));
    assert!(history.iter().any(|item| matches!(item, HistoryItem::ToolResult { output, .. } if output.contains("ERROR [tool.not_allowed_by_skill]"))));
}

#[tokio::test]
async fn scoped_text_embedded_tool_call_is_checked_before_scope_clears() {
    let fixture = PluginFixture::new("demo", "Review safely.", &["read_file"]);
    let embedded = r#"<tool_call>{"name":"list_dir","arguments":{"path":"."}}</tool_call>"#;
    let stream = Arc::new(RecordingStream::scripted(vec![
        vec![StreamPiece::ToolCall {
            id: "skill-call".into(),
            name: "skill".into(),
            arguments: serde_json::json!({"skill":"demo:inspect"}),
        }],
        vec![StreamPiece::Text(embedded.into())],
        vec![StreamPiece::Text("done".into())],
    ]));
    let session = runtime_session(&fixture, stream.clone());
    session
        .stage_plugin_snapshot(fixture.snapshot(1, true, true))
        .await
        .unwrap();
    session.prompt("inspect".into()).await.unwrap();

    let history = session.history_snapshot().await;
    assert!(history.iter().any(|item| matches!(item, HistoryItem::ToolResult { output, .. } if output.contains("ERROR [tool.not_allowed_by_skill]"))));
    let contexts = stream.contexts().await;
    assert_eq!(tool_names(&contexts[1]), vec!["read_file"]);
    assert!(tool_names(contexts.last().unwrap()).contains(&"list_dir"));
}

struct GatedRecordingStream {
    started: Semaphore,
    release: Semaphore,
    contexts: Mutex<Vec<serde_json::Value>>,
}

impl Default for GatedRecordingStream {
    fn default() -> Self {
        Self {
            started: Semaphore::new(0),
            release: Semaphore::new(0),
            contexts: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ModelStream for GatedRecordingStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), lato_core::ModelError> {
        self.contexts.lock().await.push(context);
        self.started.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        tx.send(StreamPiece::Text("done".into()))
            .await
            .map_err(|_| lato_core::ModelError::cancelled())
    }
}

#[tokio::test]
async fn staged_generation_does_not_leak_mid_turn_and_is_used_next_turn() {
    let old = PluginFixture::new("old", "Old skill.", &["read_file"]);
    let new = PluginFixture::new("new", "New skill.", &["read_file"]);
    let stream = Arc::new(GatedRecordingStream::default());
    let session = Arc::new(runtime_session(&old, stream.clone()));
    session
        .stage_plugin_snapshot(old.snapshot(1, true, true))
        .await
        .unwrap();
    fs::write(
        old.plugin.join("skills/inspect/SKILL.md"),
        "---\nname: inspect\ndescription: Mutated without reload.\n---\nchanged",
    )
    .unwrap();
    let first = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("first".into()).await }
    });
    stream.started.acquire().await.unwrap().forget();
    session
        .stage_plugin_snapshot(new.snapshot(2, true, true))
        .await
        .unwrap();
    fs::write(
        new.plugin.join("skills/inspect/SKILL.md"),
        "---\nname: inspect\ndescription: Mutated after staging.\n---\nchanged",
    )
    .unwrap();
    let first_system = stream.contexts.lock().await[0]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(first_system.contains("old:inspect"));
    assert!(first_system.contains("Old skill."));
    assert!(!first_system.contains("Mutated without reload."));
    assert!(!first_system.contains("new:inspect"));
    stream.release.add_permits(1);
    first.await.unwrap().unwrap();

    let second = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("second".into()).await }
    });
    stream.started.acquire().await.unwrap().forget();
    let second_system = stream.contexts.lock().await[1]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(second_system.contains("new:inspect"));
    assert!(second_system.contains("New skill."));
    assert!(!second_system.contains("Mutated after staging."));
    assert!(!second_system.contains("old:inspect"));
    stream.release.add_permits(1);
    second.await.unwrap().unwrap();
}

#[tokio::test]
async fn inactive_or_untrusted_plugins_inject_no_skill_text() {
    for snapshot_kind in ["inactive", "untrusted"] {
        let fixture = PluginFixture::new("demo", "Secret skill.", &["read_file"]);
        let stream = Arc::new(RecordingStream::scripted(vec![vec![StreamPiece::Text(
            "done".into(),
        )]]));
        let session = runtime_session(&fixture, stream.clone());
        let snapshot = match snapshot_kind {
            "inactive" => fixture.snapshot(1, true, false),
            "untrusted" => fixture.untrusted_project_snapshot(1),
            _ => unreachable!(),
        };
        session.stage_plugin_snapshot(snapshot).await.unwrap();
        session.prompt("hello".into()).await.unwrap();
        let system = stream.contexts().await[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(!system.contains("available_skills"));
        assert!(!system.contains("Secret skill"));
    }
}

#[tokio::test]
async fn child_catalog_and_runtime_cannot_recover_parent_only_tool() {
    let fixture = PluginFixture::new("demo", "Run parent command.", &["Bash(git diff:*)"]);
    let parent = fixture.snapshot(1, true, true);
    let child = parent.derive_child(&CapabilityCeiling {
        parent: vec![ToolCapability::FileRead, ToolCapability::ExtensionInvoke],
        profile: vec![ToolCapability::FileRead, ToolCapability::ExtensionInvoke],
        workspace: vec![ToolCapability::FileRead, ToolCapability::ExtensionInvoke],
        mcp: Default::default(),
        workflows: Default::default(),
    });
    let child_catalog = SkillCatalog::from_discovery(discover_skills(&child));
    assert!(
        child_catalog
            .render_model_listing()
            .contains("demo:inspect")
    );

    let handle = SessionSkillHandle::new(child_catalog);
    let child_runtime = builtin_tool_runtime_for_capabilities(
        BuiltinToolEnvironment {
            cwd: fixture.workspace.clone(),
            locks: Arc::new(FileLocks::new()),
            trust: SessionTrust::for_headless_prompt(&fixture.workspace),
            skill_resolver: Some(Arc::new(handle)),
        },
        Some(&[ToolCapability::FileRead, ToolCapability::ExtensionInvoke]),
    )
    .unwrap();
    assert!(!child_runtime.model_definitions().iter().any(|definition| {
        definition
            .pointer("/function/name")
            .and_then(|name| name.as_str())
            == Some("run_terminal_command")
    }));
    assert!(child_runtime.model_definitions().iter().any(|definition| {
        definition
            .pointer("/function/name")
            .and_then(|name| name.as_str())
            == Some("skill")
    }));
    let invoked = child_runtime
        .invoke(
            tool_context(),
            "skill",
            serde_json::json!({"skill":"demo:inspect"}),
        )
        .await
        .unwrap();
    let specs = serde_json::from_value::<Vec<String>>(invoked.metadata["allowedToolSpecs"].clone())
        .unwrap();
    let scope = SkillToolScope::compile(&specs, child_runtime.as_ref()).unwrap();
    assert!(
        child_runtime
            .model_definitions_scoped(Some(&scope))
            .is_empty()
    );
    assert!(
        child_runtime
            .prepare_scoped(
                tool_context(),
                "run_terminal_command",
                serde_json::json!({"command":"git diff"}),
                Some(&scope),
            )
            .is_err()
    );
}

struct AuditOrderingStream {
    store: Arc<dyn SessionStore>,
    session_id: SessionId,
    calls: Mutex<usize>,
}

#[async_trait]
impl ModelStream for AuditOrderingStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        _context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), lato_core::ModelError> {
        let mut calls = self.calls.lock().await;
        let call = *calls;
        *calls += 1;
        drop(calls);
        let replay = self.store.replay(&self.session_id).await.unwrap();
        let audits = replay
            .envelopes
            .iter()
            .filter_map(|envelope| match &envelope.record {
                JournalRecord::ExtensionAudit { audit } => Some(audit),
                _ => None,
            })
            .collect::<Vec<_>>();
        match call {
            0 => {
                assert!(matches!(
                    audits.as_slice(),
                    [ExtensionAuditRecord::SkillCatalogMaterialized { generation: 7, .. }]
                ));
                tx.send(StreamPiece::ToolCall {
                    id: "audited-skill-call".into(),
                    name: "skill".into(),
                    arguments: serde_json::json!({
                        "skill": "demo:inspect",
                        "args": "secret arguments"
                    }),
                })
                .await
                .unwrap();
            }
            1 => {
                assert!(matches!(
                    audits.as_slice(),
                    [
                        ExtensionAuditRecord::SkillCatalogMaterialized { generation: 7, .. },
                        ExtensionAuditRecord::SkillInvoked { qualified_name, .. }
                    ] if qualified_name == "demo:inspect"
                ));
                tx.send(StreamPiece::Text("done".into())).await.unwrap();
            }
            _ => panic!("unexpected model request {call}"),
        }
        Ok(())
    }
}

#[tokio::test]
async fn audit_catalog_precedes_invocation_and_invocation_precedes_next_model_request() {
    let fixture = PluginFixture::new("demo", "Review safely.", &["read_file"]);
    let session_id = SessionId::from("skill-audit-ordering");
    let store: Arc<dyn SessionStore> = Arc::new(MemoryEventStore::new());
    let stream = Arc::new(AuditOrderingStream {
        store: store.clone(),
        session_id: session_id.clone(),
        calls: Mutex::new(0),
    });
    let (updates, _) = mpsc::unbounded_channel();
    let session = RuntimeSession::new_with_store(
        session_id.to_string(),
        stream,
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(&fixture.workspace),
        fixture.workspace.clone(),
        updates,
        None,
        store.clone(),
        JournalReplay::empty(session_id.clone()),
    )
    .await
    .unwrap();
    session
        .stage_plugin_snapshot(fixture.snapshot(7, true, true))
        .await
        .unwrap();
    session
        .prompt("do not persist this prompt in audit".into())
        .await
        .unwrap();

    let replay = store.replay(&session_id).await.unwrap();
    let audits = replay
        .envelopes
        .iter()
        .filter_map(|envelope| match &envelope.record {
            JournalRecord::ExtensionAudit { audit } => Some(audit),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(audits.len(), 2);
    let encoded = serde_json::to_string(&audits).unwrap();
    for secret in [
        "secret arguments",
        "Inspect secret arguments.",
        "do not persist this prompt in audit",
    ] {
        assert!(
            !encoded.contains(secret),
            "audit leaked {secret:?}: {encoded}"
        );
    }
}

#[tokio::test]
async fn audit_rejection_hashes_requested_name_without_raw_payload() {
    let fixture = PluginFixture::new("demo", "Review safely.", &["read_file"]);
    let session_id = SessionId::from("skill-audit-rejection");
    let store: Arc<dyn SessionStore> = Arc::new(MemoryEventStore::new());
    let stream = Arc::new(RecordingStream::scripted(vec![
        vec![StreamPiece::ToolCall {
            id: "rejected-skill-call".into(),
            name: "skill".into(),
            arguments: serde_json::json!({
                "skill": "secret-missing-skill",
                "args": "secret rejected arguments"
            }),
        }],
        vec![StreamPiece::Text("done".into())],
    ]));
    let (updates, _) = mpsc::unbounded_channel();
    let session = RuntimeSession::new_with_store(
        session_id.to_string(),
        stream,
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(&fixture.workspace),
        fixture.workspace.clone(),
        updates,
        None,
        store.clone(),
        JournalReplay::empty(session_id.clone()),
    )
    .await
    .unwrap();
    session
        .stage_plugin_snapshot(fixture.snapshot(9, true, true))
        .await
        .unwrap();
    session.prompt("reject missing skill".into()).await.unwrap();

    let replay = store.replay(&session_id).await.unwrap();
    let rejected = replay.envelopes.iter().find_map(|envelope| {
        let JournalRecord::ExtensionAudit {
            audit:
                ExtensionAuditRecord::SkillRejected {
                    requested_name_hash,
                    error_code,
                    ..
                },
        } = &envelope.record
        else {
            return None;
        };
        Some((requested_name_hash, error_code, &envelope.record))
    });
    let (requested_name_hash, error_code, audit_record) = rejected.expect("skill rejection audit");
    assert!(requested_name_hash.starts_with("sha256:v1:"));
    assert_eq!(error_code, "skill.not_found");
    let encoded = serde_json::to_string(audit_record).unwrap();
    assert!(!encoded.contains("secret-missing-skill"));
    assert!(!encoded.contains("secret rejected arguments"));
}

#[tokio::test]
async fn explicit_user_skill_expands_arguments_and_enforces_first_round_scope() {
    let fixture = PluginFixture::new("demo", "Review safely.", &["read_file"]);
    let path = fixture.plugin.join("skills/inspect/SKILL.md");
    let body = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        body.replace(
            "name: inspect",
            "name: inspect\ndisable-model-invocation: true\nargument-hint: <file>",
        ),
    )
    .unwrap();
    let stream = Arc::new(RecordingStream::scripted(vec![
        vec![StreamPiece::ToolCall {
            id: "denied-user-skill-call".into(),
            name: "list_dir".into(),
            arguments: serde_json::json!({"path":"."}),
        }],
        vec![StreamPiece::Text("done".into())],
        vec![StreamPiece::Text("normal".into())],
    ]));
    let session = runtime_session(&fixture, stream.clone());
    session
        .stage_plugin_snapshot(fixture.snapshot(7, true, true))
        .await
        .unwrap();
    let listing = session.list_skills().await;
    assert_eq!(listing["generation"], 7);
    assert_eq!(listing["skills"][0]["qualifiedName"], "demo:inspect");
    assert_eq!(listing["skills"][0]["argumentHint"], "<file>");
    session
        .prompt_skill("demo:inspect".into(), Some("目标.txt".into()))
        .await
        .unwrap();
    session.prompt("hello".into()).await.unwrap();
    let contexts = stream.contexts().await;
    assert_eq!(tool_names(&contexts[0]), vec!["read_file"]);
    assert!(
        contexts[0]["messages"]
            .to_string()
            .contains("Inspect 目标.txt.")
    );
    assert!(tool_names(&contexts[1]).contains(&"list_dir"));
    assert!(tool_names(&contexts[2]).contains(&"list_dir"));
    let history = session.history_snapshot().await;
    assert!(history.iter().any(|item| matches!(item, HistoryItem::ToolResult { output, .. } if output.contains("tool.not_allowed_by_skill"))));
}

#[tokio::test]
async fn explicit_user_skill_rejects_hidden_skills_and_recovers_for_next_prompt() {
    let fixture = PluginFixture::new("demo", "Internal only.", &["read_file"]);
    let path = fixture.plugin.join("skills/inspect/SKILL.md");
    let body = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        body.replace("name: inspect", "name: inspect\nuser-invocable: false"),
    )
    .unwrap();
    let stream = Arc::new(RecordingStream::scripted(vec![vec![StreamPiece::Text(
        "normal".into(),
    )]]));
    let session = runtime_session(&fixture, stream.clone());
    session
        .stage_plugin_snapshot(fixture.snapshot(8, true, true))
        .await
        .unwrap();
    assert_eq!(session.list_skills().await["skills"], serde_json::json!([]));
    let error = session
        .prompt_skill("demo:inspect".into(), None)
        .await
        .unwrap_err();
    assert!(error.message.contains("cannot be invoked by a user"));
    assert!(stream.contexts().await.is_empty());
    session.prompt("hello".into()).await.unwrap();
    assert!(tool_names(&stream.contexts().await[0]).contains(&"list_dir"));
}

#[tokio::test]
async fn session_lists_trusted_workflows_from_the_current_snapshot() {
    let fixture = PluginFixture::new("demo", "Inspect.", &["read_file"]);
    fs::write(
        fixture.plugin.join("plugin.json"),
        r#"{"name":"demo","skills":"skills","workflows":{"review":{"description":"Review the diff","prompt":"Review"}}}"#,
    )
    .unwrap();
    let session = runtime_session(&fixture, Arc::new(RecordingStream::default()));
    session
        .stage_plugin_snapshot(fixture.snapshot(3, true, true))
        .await
        .unwrap();
    let listing = session.list_workflows().await;
    assert_eq!(listing["generation"], 3);
    assert_eq!(listing["workflows"][0]["id"], "demo/review");
    assert_eq!(listing["workflows"][0]["description"], "Review the diff");
    session
        .stage_plugin_snapshot(fixture.snapshot(4, true, false))
        .await
        .unwrap();
    assert_eq!(
        session.list_workflows().await["workflows"],
        serde_json::json!([])
    );
}

#[tokio::test]
async fn cancelling_user_skill_clears_scope_before_next_prompt() {
    let fixture = PluginFixture::new("demo", "Review safely.", &["read_file"]);
    let stream = Arc::new(GatedRecordingStream::default());
    let session = Arc::new(runtime_session(&fixture, stream.clone()));
    session
        .stage_plugin_snapshot(fixture.snapshot(9, true, true))
        .await
        .unwrap();
    let running = {
        let session = session.clone();
        tokio::spawn(async move { session.prompt_skill("demo:inspect".into(), None).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), stream.started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    session.cancel().await.unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        outcome,
        lato_agent::RuntimePromptOutcome::Cancelled { .. }
    ));
    // Legacy ModelStream producers finish independently after cancellation.
    stream.release.add_permits(2);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        session.prompt("hello".into()),
    )
    .await
    .unwrap()
    .unwrap();
    let contexts = stream.contexts.lock().await;
    assert_eq!(tool_names(&contexts[0]), vec!["read_file"]);
    assert!(tool_names(&contexts[1]).contains(&"list_dir"));
}

#[tokio::test]
async fn positional_user_skill_preserves_separate_file_context_and_scope() {
    let fixture = PluginFixture::new("demo", "Review safely.", &["read_file"]);
    let path = fixture.plugin.join("skills/inspect/SKILL.md");
    let body = fs::read_to_string(&path).unwrap();
    fs::write(&path, body.replace("Inspect $ARGUMENTS.", "Inspect $1.")).unwrap();
    let stream = Arc::new(RecordingStream::scripted(vec![vec![StreamPiece::Text(
        "done".into(),
    )]]));
    let session = runtime_session(&fixture, stream.clone());
    session
        .stage_plugin_snapshot(fixture.snapshot(10, true, true))
        .await
        .unwrap();
    let attachment =
        "<file path=\"目标.rs\">\nconst TEXT: &str = \"$ARGUMENTS $(shell)\";\n</file>";
    session
        .prompt_skill_with_context(
            "demo:inspect".into(),
            Some("unused target.rs".into()),
            Some(attachment.into()),
        )
        .await
        .unwrap();
    let contexts = stream.contexts().await;
    assert_eq!(tool_names(&contexts[0]), vec!["read_file"]);
    let messages = contexts[0]["messages"].as_array().unwrap();
    let user = messages
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(user.contains("Inspect target.rs."));
    assert!(user.ends_with(attachment));
    assert!(user.find("</skill>").unwrap() < user.find("<file path=").unwrap());
}
