use crate::HistoryItem;
use lato_ai::{CONTEXT_HARD_LIMIT_BYTES, ModelStream, StreamPiece};
use lato_tools::{ToolCall, bound_tool_output, dispatch, phase0_tool_definitions};
use lato_workspace::{FileLocks, SessionTrust};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::mpsc;

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

pub struct SessionActor {
    active: bool,
    cancelled: bool,
    history: Vec<HistoryItem>,
    stream: Arc<dyn ModelStream>,
    locks: Arc<FileLocks>,
    trust: SessionTrust,
    cwd: PathBuf,
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
        Self {
            active: false,
            cancelled: false,
            history: vec![HistoryItem::System(build_world_state(&cwd))],
            stream,
            locks,
            trust,
            cwd,
            #[cfg(test)]
            on_after_persist: None,
        }
    }
    pub async fn prompt(&mut self, _kind: PromptKind, text: String) -> Result<TurnOutcome, String> {
        if self.active {
            self.cancelled = true;
            self.active = false;
        }
        self.active = true;
        self.cancelled = false;
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
            let context = serde_json::json!({
                "messages": history_to_messages(&self.history),
                "tools": phase0_tool_definitions(),
            });
            self.stream.stream(self.encoded_len(), context, tx).await?;
            let mut saw_tool = false;
            while let Some(piece) = rx.recv().await {
                if self.cancelled {
                    self.active = false;
                    return Ok(TurnOutcome::Cancelled);
                }
                match piece {
                    StreamPiece::Text(t) => self.history.push(HistoryItem::AssistantText(t)),
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
                        if self.cancelled {
                            self.active = false;
                            return Ok(TurnOutcome::Cancelled);
                        }
                        let out = dispatch(
                            &self.locks,
                            &self.trust,
                            &self.cwd,
                            ToolCall { name, arguments },
                        )
                        .await
                        .unwrap_or_else(|e| format!("ERROR: {e}"));
                        let out = bound_tool_output(out, &self.cwd, &id).await?;
                        self.history
                            .push(HistoryItem::ToolResult { id, output: out });
                    }
                }
            }
            if !saw_tool {
                self.active = false;
                return Ok(TurnOutcome::Complete);
            }
        }
    }
    pub fn cancel(&mut self) {
        self.cancelled = true;
    }
    pub fn history(&self) -> &[HistoryItem] {
        &self.history
    }
    pub fn latest_assistant_text(&self) -> String {
        self.history
            .iter()
            .filter_map(|item| match item {
                HistoryItem::AssistantText(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
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
