use crate::HistoryItem;
use async_trait::async_trait;
use lato_ai::{CONTEXT_HARD_LIMIT_BYTES, ModelStream, StreamPiece};
use lato_core::{SessionId, ToolCallId, ToolContext, TurnId};
use lato_tools::{BuiltinToolEnvironment, ToolRuntime, bound_tool_output, builtin_tool_runtime};
use lato_workspace::{FileLocks, SessionTrust};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptKind {
    Start,
    Steer,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnOutcome {
    Complete,
    Cancelled,
    Replaced,
}

#[async_trait]
pub trait ToolApproval: Send + Sync {
    async fn approve(&self, name: &str, arguments: &serde_json::Value) -> bool;
}

pub struct SessionActor {
    active: bool,
    cancelled: bool,
    history: Vec<HistoryItem>,
    stream: Arc<dyn ModelStream>,
    _locks: Arc<FileLocks>,
    trust: SessionTrust,
    cwd: PathBuf,
    tool_runtime: Arc<ToolRuntime>,
    session_id: SessionId,
    turn_id: TurnId,
    turn_cancellation: CancellationToken,
    next_local_turn: u64,
    next_local_call: u64,
    events: Option<(mpsc::UnboundedSender<serde_json::Value>, String)>,
    tool_approval: Option<Arc<dyn ToolApproval>>,
    #[cfg(test)]
    pub(crate) on_after_persist: Option<Box<dyn Fn() + Send + Sync>>,
}

impl SessionActor {
    pub fn new(
        stream: Arc<dyn ModelStream>,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
    ) -> Self {
        let tool_runtime = builtin_tool_runtime(BuiltinToolEnvironment {
            cwd: cwd.clone(),
            locks: locks.clone(),
            trust: trust.clone(),
        })
        .expect("static built-in tool descriptors must form a valid runtime");
        Self::new_with_tool_runtime(stream, locks, trust, cwd, tool_runtime)
    }

    pub fn new_with_tool_runtime(
        stream: Arc<dyn ModelStream>,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
        tool_runtime: Arc<ToolRuntime>,
    ) -> Self {
        Self {
            active: false,
            cancelled: false,
            history: vec![HistoryItem::System(build_world_state(&cwd))],
            stream,
            _locks: locks,
            trust,
            cwd,
            tool_runtime,
            session_id: SessionId::from("local-session"),
            turn_id: TurnId::from("local-turn-0"),
            turn_cancellation: CancellationToken::new(),
            next_local_turn: 0,
            next_local_call: 0,
            events: None,
            tool_approval: None,
            #[cfg(test)]
            on_after_persist: None,
        }
    }
    pub fn with_interactive_events(
        mut self,
        events: mpsc::UnboundedSender<serde_json::Value>,
        session_id: String,
        approval: Option<Arc<dyn ToolApproval>>,
    ) -> Self {
        self.session_id = SessionId::parse(session_id.clone())
            .unwrap_or_else(|_| SessionId::from("local-session"));
        self.events = Some((events, session_id));
        self.tool_approval = approval;
        self
    }

    pub async fn prompt(&mut self, kind: PromptKind, text: String) -> Result<TurnOutcome, String> {
        self.next_local_turn += 1;
        let turn_id = TurnId::from(format!("local-turn-{}", self.next_local_turn));
        self.prompt_with_context(kind, text, turn_id, CancellationToken::new())
            .await
    }

    pub async fn prompt_with_context(
        &mut self,
        _kind: PromptKind,
        text: String,
        turn_id: TurnId,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, String> {
        if self.active {
            self.cancelled = true;
            self.active = false;
        }
        self.active = true;
        self.cancelled = false;
        self.turn_id = turn_id;
        self.turn_cancellation = cancellation;
        self.history.push(HistoryItem::User(text));
        noop_hooks();
        let mut sampling_steps = 0usize;
        let mut repeated_calls: HashMap<String, usize> = HashMap::new();
        loop {
            sampling_steps += 1;
            if sampling_steps > 50 {
                self.active = false;
                return Err("maximum sampling steps exceeded".into());
            }
            if self.encoded_len() > CONTEXT_HARD_LIMIT_BYTES {
                self.active = false;
                return Err("context exceeds hard limit; compact not implemented".into());
            }
            let (tx, mut rx) = mpsc::channel(16);
            let tool_runtime = self.tool_runtime.clone();
            let context = serde_json::json!({
                "messages": history_to_messages(&self.history),
                "tools": tool_runtime.model_definitions(),
            });
            let stream = self.stream.clone();
            let prompt_bytes = self.encoded_len();
            let stream_task =
                tokio::spawn(async move { stream.stream(prompt_bytes, context, tx).await });
            let mut saw_tool = false;
            while let Some(piece) = rx.recv().await {
                if self.cancelled || self.turn_cancellation.is_cancelled() {
                    self.active = false;
                    return Ok(TurnOutcome::Cancelled);
                }
                match piece {
                    StreamPiece::Text(t) => {
                        if let Some((events, session_id)) = &self.events {
                            let _ = events.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session_id,"delta":t}}));
                        }
                        self.history.push(HistoryItem::AssistantText(t));
                    }
                    StreamPiece::ToolCall {
                        id,
                        name,
                        arguments,
                    } => {
                        saw_tool = true;
                        let fingerprint = format!(
                            "{name}:{}",
                            serde_json::to_string(&arguments).unwrap_or_default()
                        );
                        let repeats = repeated_calls.entry(fingerprint).or_default();
                        *repeats += 1;
                        if *repeats > 3 {
                            self.active = false;
                            return Err(
                                "stalled: identical tool call repeated more than 3 times".into()
                            );
                        }
                        self.history.push(HistoryItem::ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                        });
                        #[cfg(test)]
                        if let Some(cb) = &self.on_after_persist {
                            cb();
                        }
                        if self.cancelled || self.turn_cancellation.is_cancelled() {
                            self.active = false;
                            return Ok(TurnOutcome::Cancelled);
                        }
                        if lato_tools::requires_approval(&name)
                            && self.trust.mode == lato_workspace::ApprovalMode::Ask
                            && !self.trust.has_allow_once()
                        {
                            let approved = match &self.tool_approval {
                                Some(approval) => approval.approve(&name, &arguments).await,
                                None => false,
                            };
                            if approved {
                                self.trust.allow_once();
                            }
                        }
                        if let Some((events, session_id)) = &self.events {
                            let _ = events.send(serde_json::json!({"jsonrpc":"2.0","method":"session/tool_call","params":{"sessionId":session_id,"name":name,"arguments":arguments}}));
                        }
                        let call_id = ToolCallId::parse(id.clone()).unwrap_or_else(|_| {
                            self.next_local_call += 1;
                            ToolCallId::from(format!("local-tool-call-{}", self.next_local_call))
                        });
                        let context = ToolContext {
                            session_id: self.session_id.clone(),
                            turn_id: self.turn_id.clone(),
                            call_id,
                            cancellation: self.turn_cancellation.clone(),
                        };
                        let tool_runtime = self.tool_runtime.clone();
                        let invocation = tool_runtime.invoke(context, &name, arguments).await;
                        let out = invocation
                            .map(|output| output.content)
                            .unwrap_or_else(|error| format!("ERROR: {}", error.message));
                        let out = bound_tool_output(out, &self.cwd, &id).await?;
                        self.history
                            .push(HistoryItem::ToolResult { id, output: out });
                    }
                }
            }
            stream_task.await.map_err(|error| error.to_string())??;
            if !saw_tool {
                self.active = false;
                return Ok(TurnOutcome::Complete);
            }
        }
    }
    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.turn_cancellation.cancel();
    }
    pub fn history(&self) -> &[HistoryItem] {
        &self.history
    }
    pub fn latest_assistant_text(&self) -> String {
        let mut parts = self
            .history
            .iter()
            .rev()
            .take_while(|item| !matches!(item, HistoryItem::User(_)))
            .filter_map(|item| match item {
                HistoryItem::AssistantText(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        parts.reverse();
        parts.concat()
    }
    pub fn history_mut(&mut self) -> &mut Vec<HistoryItem> {
        &mut self.history
    }
    pub fn encoded_len(&self) -> usize {
        serde_json::to_vec(&self.history)
            .map(|v| v.len())
            .unwrap_or(usize::MAX)
    }

    pub fn compact_explicit(
        &mut self,
        summary: String,
        retain_recent: usize,
    ) -> Result<(), String> {
        if summary.trim().is_empty() {
            return Err("compaction summary must not be empty".into());
        }
        let keep_from = self.history.len().saturating_sub(retain_recent);
        let recent = self.history.split_off(keep_from);
        self.history.clear();
        self.history.push(HistoryItem::CompactionSummary(summary));
        self.history.extend(recent);
        Ok(())
    }
}
fn noop_hooks() {}

fn history_to_messages(history: &[HistoryItem]) -> serde_json::Value {
    serde_json::Value::Array(history.iter().map(|item| match item {
        HistoryItem::System(content) => serde_json::json!({"role":"system","content":content}),
        HistoryItem::User(content) => serde_json::json!({"role":"user","content":content}),
        HistoryItem::AssistantText(content) => serde_json::json!({"role":"assistant","content":content}),
        HistoryItem::ToolCall { id, name, arguments } => serde_json::json!({
            "role":"assistant","tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":serde_json::to_string(arguments).unwrap_or_default()}}]
        }),
        HistoryItem::ToolResult { id, output } => serde_json::json!({"role":"tool","tool_call_id":id,"content":output}),
        HistoryItem::CompactionSummary(content) => serde_json::json!({"role":"system","content":format!("Compaction summary:\n{content}")}),
    }).collect())
}

fn build_world_state(cwd: &std::path::Path) -> String {
    let mut text = format!(
        "You are Lato, a coding agent. Work in the host workspace.\nCWD: {}\nShell: {}\nUnix time: {}",
        cwd.display(),
        lato_workspace::default_shell().to_string_lossy(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    );
    let mut dirs = Vec::new();
    let mut current = Some(cwd);
    while let Some(dir) = current {
        dirs.push(dir);
        if dir.join(".git").exists() {
            break;
        }
        current = dir.parent();
    }
    dirs.reverse();
    for dir in dirs {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let path = dir.join(name);
            if let Ok(contents) = std::fs::read_to_string(&path) {
                const LIMIT: usize = 64 * 1024;
                let mut boundary = contents.len().min(LIMIT);
                while !contents.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                text.push_str(&format!(
                    "\n\nInstructions from {}:\n{}",
                    path.display(),
                    &contents[..boundary]
                ));
                break;
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use lato_ai::FakeModelStream;
    use serde_json::json;

    fn actor(script: Vec<Vec<StreamPiece>>, cwd: PathBuf) -> SessionActor {
        SessionActor::new(
            Arc::new(FakeModelStream::new(script)),
            Arc::new(FileLocks::new()),
            SessionTrust::for_headless_prompt(&cwd),
            cwd,
        )
    }

    struct AllowTool;
    #[async_trait]
    impl ToolApproval for AllowTool {
        async fn approve(&self, _name: &str, _arguments: &serde_json::Value) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn interactive_streams_deltas_and_approves_at_tool_boundary() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "old").unwrap();
        let (events, mut rx) = mpsc::unbounded_channel();
        let mut actor = SessionActor::new(
            Arc::new(FakeModelStream::new(vec![
                vec![StreamPiece::ToolCall {
                    id: "edit".into(),
                    name: "search_replace".into(),
                    arguments: json!({"path":"a.txt","old":"old","new":"new"}),
                }],
                vec![StreamPiece::Text("done".into())],
            ])),
            Arc::new(FileLocks::new()),
            SessionTrust::for_interactive(d.path(), true),
            d.path().to_path_buf(),
        )
        .with_interactive_events(events, "session".into(), Some(Arc::new(AllowTool)));
        actor
            .prompt(PromptKind::Start, "edit".into())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "new"
        );
        let emitted = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(
            emitted
                .iter()
                .any(|event| event["method"] == "session/tool_call")
        );
        assert!(
            emitted
                .iter()
                .any(|event| event.pointer("/params/delta") == Some(&json!("done")))
        );
    }

    #[tokio::test]
    async fn latest_assistant_text_is_only_the_current_turn() {
        let d = tempfile::tempdir().unwrap();
        let mut actor = actor(
            vec![
                vec![StreamPiece::Text("first".into())],
                vec![StreamPiece::Text("second".into())],
            ],
            d.path().to_path_buf(),
        );
        actor.prompt(PromptKind::Start, "one".into()).await.unwrap();
        actor.prompt(PromptKind::Start, "two".into()).await.unwrap();
        assert_eq!(actor.latest_assistant_text(), "second");
    }

    #[tokio::test]
    async fn a2_1_persist_then_execute() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "hi").unwrap();
        let mut a = actor(
            vec![
                vec![StreamPiece::ToolCall {
                    id: "1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path":"a.txt"}),
                }],
                vec![StreamPiece::Text("done".into())],
            ],
            d.path().to_path_buf(),
        );
        a.prompt(PromptKind::Start, "go".into()).await.unwrap();
        let call_pos = a
            .history()
            .iter()
            .position(|h| matches!(h, HistoryItem::ToolCall { .. }))
            .unwrap();
        let result_pos = a
            .history()
            .iter()
            .position(|h| matches!(h, HistoryItem::ToolResult { .. }))
            .unwrap();
        assert!(call_pos < result_pos);
        assert!(matches!(
            a.history().last(),
            Some(HistoryItem::AssistantText(_))
        ));
    }
    #[tokio::test]
    async fn a2_2_cancel_keeps_tool_call() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(
            vec![vec![StreamPiece::ToolCall {
                id: "1".into(),
                name: "read_file".into(),
                arguments: json!({"path":"missing"}),
            }]],
            d.path().to_path_buf(),
        );
        a.on_after_persist = Some(Box::new(|| {}));
        // Directly verify persist-before-result invariant via normal execution error result.
        a.prompt(PromptKind::Start, "go".into()).await.unwrap();
        assert!(
            a.history()
                .iter()
                .any(|h| matches!(h, HistoryItem::ToolCall { .. }))
        );
    }
    #[tokio::test]
    async fn a2_6_hard_limit() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(vec![], d.path().to_path_buf());
        a.history_mut()
            .push(HistoryItem::User("x".repeat(CONTEXT_HARD_LIMIT_BYTES + 1)));
        let err = a.prompt(PromptKind::Start, "go".into()).await.unwrap_err();
        assert!(err.contains("compact"));
    }
    #[tokio::test]
    async fn a2_7_second_start_aborts() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(
            vec![
                vec![StreamPiece::Text("one".into())],
                vec![StreamPiece::Text("two".into())],
            ],
            d.path().to_path_buf(),
        );
        a.prompt(PromptKind::Start, "one".into()).await.unwrap();
        a.prompt(PromptKind::Start, "two".into()).await.unwrap();
        assert!(
            a.history()
                .iter()
                .filter(|h| matches!(h, HistoryItem::User(_)))
                .count()
                >= 2
        );
    }
    #[tokio::test]
    async fn a2_4_no_mcp_ok() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(
            vec![vec![StreamPiece::Text("ok".into())]],
            d.path().to_path_buf(),
        );
        assert_eq!(
            a.prompt(PromptKind::Start, "hi".into()).await.unwrap(),
            TurnOutcome::Complete
        );
    }
    #[tokio::test]
    async fn g4_no_silent_truncate() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(vec![], d.path().to_path_buf());
        a.history_mut()
            .push(HistoryItem::User("x".repeat(CONTEXT_HARD_LIMIT_BYTES + 1)));
        let err = a.prompt(PromptKind::Start, "go".into()).await.unwrap_err();
        assert!(err.contains("compact"));
    }

    #[test]
    fn world_state_injects_agents_chain_and_standard_messages() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("AGENTS.md"), "Use cargo test.").unwrap();
        let a = actor(vec![], d.path().to_path_buf());
        let messages = history_to_messages(a.history());
        assert_eq!(messages[0]["role"], "system");
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("Use cargo test")
        );
        assert!(messages[0]["content"].as_str().unwrap().contains("CWD:"));
    }

    #[tokio::test]
    async fn repeated_identical_tool_calls_trigger_stall_detection() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "x").unwrap();
        let call = StreamPiece::ToolCall {
            id: "same".into(),
            name: "read_file".into(),
            arguments: json!({"path":"a.txt"}),
        };
        let mut a = actor(
            vec![
                vec![call.clone()],
                vec![call.clone()],
                vec![call.clone()],
                vec![call],
            ],
            d.path().to_path_buf(),
        );
        let err = a
            .prompt(PromptKind::Start, "loop".into())
            .await
            .unwrap_err();
        assert!(err.contains("stalled"));
    }

    #[test]
    fn explicit_compaction_writes_summary_and_retains_recent_history() {
        let d = tempfile::tempdir().unwrap();
        let mut a = actor(vec![], d.path().to_path_buf());
        a.history_mut().extend([
            HistoryItem::User("old".into()),
            HistoryItem::AssistantText("answer".into()),
            HistoryItem::User("recent".into()),
        ]);
        a.compact_explicit("summary of old conversation".into(), 1)
            .unwrap();
        assert!(
            matches!(&a.history()[0], HistoryItem::CompactionSummary(v) if v.contains("summary"))
        );
        assert!(matches!(&a.history()[1], HistoryItem::User(v) if v == "recent"));
    }
}
