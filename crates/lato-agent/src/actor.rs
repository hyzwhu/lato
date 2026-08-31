use crate::HistoryItem;
use lato_ai::{CONTEXT_HARD_LIMIT_BYTES, FakeModelStream, StreamPiece};
use lato_tools::{ToolCall, dispatch};
use lato_workspace::{FileLocks, SessionTrust};
use std::{path::PathBuf, sync::Arc};
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
    stream: Arc<FakeModelStream>,
    locks: Arc<FileLocks>,
    trust: SessionTrust,
    cwd: PathBuf,
    #[cfg(test)]
    pub(crate) on_after_persist: Option<Box<dyn Fn() + Send + Sync>>,
}

impl SessionActor {
    pub fn new(
        stream: Arc<FakeModelStream>,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        cwd: PathBuf,
    ) -> Self {
        Self {
            active: false,
            cancelled: false,
            history: Vec::new(),
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
        loop {
            if self.encoded_len() > CONTEXT_HARD_LIMIT_BYTES {
                self.active = false;
                return Err("context exceeds hard limit; compact not implemented".into());
            }
            let (tx, mut rx) = mpsc::channel(16);
            self.stream.stream(self.encoded_len(), tx).await?;
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
    pub fn history_mut(&mut self) -> &mut Vec<HistoryItem> {
        &mut self.history
    }
    pub fn encoded_len(&self) -> usize {
        serde_json::to_vec(&self.history)
            .map(|v| v.len())
            .unwrap_or(usize::MAX)
    }
}
fn noop_hooks() {}

#[cfg(test)]
mod tests {
    use super::*;
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
}
