use super::{
    TuiExit,
    backend::{BackendCommand, BackendEvent},
    i18n::Language,
    input::InputBuffer,
    tool_panel::ToolPanelState,
};
use crate::client::{ClientUpdate, CompactionSizeResponse, SessionSummary};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

const DELETE_CONFIRM_WINDOW: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Screen {
    Welcome,
    Main,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Focus {
    Sessions,
    Chat,
    Tools,
}

impl Focus {
    pub fn next(self) -> Self {
        match self {
            Self::Sessions => Self::Chat,
            Self::Chat => Self::Tools,
            Self::Tools => Self::Sessions,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Sessions => Self::Tools,
            Self::Chat => Self::Sessions,
            Self::Tools => Self::Chat,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutMode {
    Wide,
    Medium,
    Narrow,
    TooSmall,
}

impl LayoutMode {
    pub fn for_size(width: u16, height: u16) -> Self {
        if width < 46 || height < 14 {
            Self::TooSmall
        } else if width < 72 {
            Self::Narrow
        } else if width < 105 {
            Self::Medium
        } else {
            Self::Wide
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Overlay {
    CommandPalette,
    Search,
    Configuration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageRole {
    User,
    Assistant,
    Reasoning,
    System,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
    pub expanded: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolStatus {
    Running,
    Done,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionUiState {
    Idle,
    Running {
        started_at: Instant,
        trigger: String,
    },
    Completed {
        before: CompactionSizeResponse,
        after: CompactionSizeResponse,
        checkpoint_id: String,
        warning: Option<lato_core::AgentError>,
    },
    Failed {
        code: String,
        message: String,
    },
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextUiState {
    pub estimated_input_tokens: u64,
    pub context_window: Option<u64>,
    pub utilization_percent: Option<u8>,
}

#[derive(Clone, Debug)]
pub struct ToolCard {
    pub expanded: bool,
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub status: ToolStatus,
    pub result: Option<String>,
    pub started_at: Instant,
    pub elapsed_ms: u128,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionItem {
    pub id: String,
    pub title: String,
    pub timestamp: String,
}

impl From<SessionSummary> for SessionItem {
    fn from(summary: SessionSummary) -> Self {
        let timestamp = relative_timestamp(summary.updated_at_ms);
        Self {
            id: summary.session_id,
            title: summary.title,
            timestamp,
        }
    }
}

fn relative_timestamp(timestamp_ms: u64) -> String {
    if timestamp_ms == 0 {
        return String::new();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let seconds = now.saturating_sub(timestamp_ms) / 1_000;
    match seconds {
        0..=59 => "now".into(),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

#[derive(Debug)]
pub struct ApprovalState {
    pub tool: String,
    pub summary: String,
    pub response: tokio::sync::oneshot::Sender<bool>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GenerationPhase {
    #[default]
    Idle,
    Waiting,
    Thinking,
    Tools,
    Answering,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug)]
pub struct ReasoningSegment {
    pub message_index: usize,
    pub started_at: Instant,
    pub duration: Option<Duration>,
}

#[derive(Debug)]
pub struct AppState {
    pub screen: Screen,
    pub language: Language,
    pub workspace: PathBuf,
    pub model: String,
    pub session_id: String,
    pub sessions: Vec<SessionItem>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolCard>,
    pub tool_panel: ToolPanelState,
    pub composer: InputBuffer,
    pub search: InputBuffer,
    pub palette_query: InputBuffer,
    pub files: Vec<String>,
    pub files_loading: bool,
    pub files_error: Option<String>,
    pub skills: Vec<crate::client::SkillEntry>,
    pub skills_error: Option<String>,
    pub skills_loading: bool,
    pub workflows: Vec<crate::client::WorkflowEntry>,
    pub workflow_runs: Vec<crate::client::WorkflowRunView>,
    pub history: Vec<String>,
    pub history_index: Option<usize>,
    pub history_draft: String,
    pub parked_draft: Option<String>,
    pub focus: Focus,
    pub overlay: Option<Overlay>,
    pub palette_index: usize,
    pub slash_completion_index: usize,
    slash_completion_dismissed: bool,
    pub approval: Option<ApprovalState>,
    pub layout: LayoutMode,
    pub responding: bool,
    pub generation_phase: GenerationPhase,
    pub reasoning_segments: Vec<ReasoningSegment>,
    pub active_reasoning: Option<usize>,
    pub reveal_reasoning: Option<usize>,
    pub turn_message_start: usize,
    pub animation_frame: usize,
    pub compaction: CompactionUiState,
    pub context_usage: Option<ContextUiState>,
    pub recovery_suppression: String,
    pub response_started: Option<Instant>,
    pub elapsed_seconds: u64,
    pub error: Option<String>,
    pub scroll: u16,
    pub scroll_max: u16,
    pub session_index: usize,
    pub armed_session_delete: Option<(String, Instant)>,
    pub should_exit: bool,
    pub exit_action: TuiExit,
}

#[derive(Debug)]
pub enum AppEvent {
    Submit,
    Backend(BackendEvent),
    Tick,
    Resize(u16, u16),
    FocusNext,
    FocusPrevious,
    TogglePalette,
    OpenSearch,
    Escape,
    Scroll(i16),
    SwitchLanguage(Language),
    NewSession,
    Exit(TuiExit),
}

#[derive(Debug)]
pub enum Effect {
    Backend(BackendCommand),
    PersistLanguage(Language),
    PersistModel(String),
    ConfigureModel,
    Doctor,
    Sessions,
    RenameSession { session_id: String, title: String },
    ConfirmDeleteSession(String),
    Login,
    PrepareSubmit(String),
    PrepareSkill { name: String, args: String },
}

impl AppState {
    pub fn new(
        language: Language,
        workspace: PathBuf,
        model: String,
        session_id: String,
        sessions: Vec<String>,
    ) -> Self {
        let sessions = sessions
            .into_iter()
            .map(|id| SessionItem {
                title: compact_session_title(&id),
                id,
                timestamp: String::new(),
            })
            .collect();
        Self {
            screen: Screen::Welcome,
            language,
            workspace,
            model,
            session_id,
            sessions,
            messages: Vec::new(),
            tools: Vec::new(),
            tool_panel: ToolPanelState::default(),
            composer: InputBuffer::new(),
            search: InputBuffer::new(),
            palette_query: InputBuffer::new(),
            files: Vec::new(),
            files_loading: false,
            files_error: None,
            skills: Vec::new(),
            skills_loading: true,
            skills_error: None,
            workflows: Vec::new(),
            workflow_runs: Vec::new(),
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            parked_draft: None,
            focus: Focus::Chat,
            overlay: None,
            palette_index: 0,
            slash_completion_index: 0,
            slash_completion_dismissed: false,
            approval: None,
            layout: LayoutMode::Wide,
            responding: false,
            generation_phase: GenerationPhase::Idle,
            reasoning_segments: Vec::new(),
            active_reasoning: None,
            reveal_reasoning: None,
            turn_message_start: 0,
            animation_frame: 0,
            compaction: CompactionUiState::Idle,
            context_usage: None,
            recovery_suppression: "none".into(),
            response_started: None,
            elapsed_seconds: 0,
            error: None,
            scroll: 0,
            scroll_max: 0,
            session_index: 0,
            armed_session_delete: None,
            should_exit: false,
            exit_action: TuiExit::Quit,
        }
    }

    pub fn completion_dismissed(&self) -> bool {
        self.slash_completion_dismissed
    }

    #[cfg(test)]
    pub fn slash_completion(&self) -> Vec<&'static super::commands::SlashCommand> {
        if self.slash_completion_dismissed || self.focus != Focus::Chat {
            Vec::new()
        } else {
            super::commands::matches(self.composer.as_str())
        }
    }

    pub fn refresh_slash_completion(&mut self) {
        self.slash_completion_dismissed = false;
        let last = self.candidates().len().saturating_sub(1);
        self.slash_completion_index = self.slash_completion_index.min(last);
    }

    pub fn dismiss_slash_completion(&mut self) {
        self.slash_completion_dismissed = true;
    }

    pub fn new_with_summaries(
        language: Language,
        workspace: PathBuf,
        model: String,
        session_id: String,
        sessions: Vec<SessionSummary>,
    ) -> Self {
        let mut app = Self::new(language, workspace, model, session_id, Vec::new());
        app.sessions = sessions.into_iter().map(SessionItem::from).collect();
        app.select_current_session();
        app
    }

    pub fn arm_or_confirm_selected_delete(&mut self) -> Option<String> {
        if self.is_busy() {
            self.error = Some(
                "Wait for the current response or cancel it first / 请先等待或取消当前回复".into(),
            );
            return None;
        }
        let id = self.sessions.get(self.session_index)?.id.clone();
        let confirmed = self
            .armed_session_delete
            .as_ref()
            .is_some_and(|(armed, at)| armed == &id && at.elapsed() <= DELETE_CONFIRM_WINDOW);
        if confirmed {
            self.armed_session_delete = None;
            Some(id)
        } else {
            self.armed_session_delete = Some((id, Instant::now()));
            None
        }
    }

    pub fn disarm_session_delete(&mut self) {
        self.armed_session_delete = None;
    }

    fn replace_sessions(&mut self, summaries: Vec<SessionSummary>) {
        let selected = self
            .sessions
            .get(self.session_index)
            .map(|session| session.id.clone());
        self.sessions = summaries.into_iter().map(SessionItem::from).collect();
        self.session_index = selected
            .as_deref()
            .and_then(|id| self.sessions.iter().position(|session| session.id == id))
            .unwrap_or_else(|| {
                self.sessions
                    .iter()
                    .position(|session| session.id == self.session_id)
                    .unwrap_or(0)
            });
        self.disarm_session_delete();
    }

    fn select_current_session(&mut self) {
        if let Some(index) = self
            .sessions
            .iter()
            .position(|session| session.id == self.session_id)
        {
            self.session_index = index;
        }
    }

    pub fn reduce(&mut self, event: AppEvent) -> Vec<Effect> {
        match event {
            AppEvent::Submit => self.submit(),
            AppEvent::Backend(BackendEvent::ModelSwitched(response)) => {
                let selection = format!("{}/{}", response.provider, response.model);
                if self.model != selection {
                    self.context_usage = None;
                }
                self.model = selection.clone();
                self.error = response.compaction_warning.map(|warning| warning.message);
                vec![Effect::PersistModel(selection)]
            }
            AppEvent::Backend(event) => {
                self.apply_backend(event);
                Vec::new()
            }
            AppEvent::Tick => {
                self.animation_frame = self.animation_frame.wrapping_add(1);
                if let Some(started) = self.response_started {
                    self.elapsed_seconds = started.elapsed().as_secs();
                }
                for tool in &mut self.tools {
                    if tool.status == ToolStatus::Running {
                        tool.elapsed_ms = tool.started_at.elapsed().as_millis();
                    }
                }
                if self
                    .armed_session_delete
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() > DELETE_CONFIRM_WINDOW)
                {
                    self.disarm_session_delete();
                }
                Vec::new()
            }
            AppEvent::Resize(width, height) => {
                self.layout = LayoutMode::for_size(width, height);
                Vec::new()
            }
            AppEvent::FocusNext => {
                self.disarm_session_delete();
                self.focus = self.focus.next();
                Vec::new()
            }
            AppEvent::FocusPrevious => {
                self.disarm_session_delete();
                self.focus = self.focus.previous();
                Vec::new()
            }
            AppEvent::TogglePalette => {
                self.palette_query.clear();
                self.palette_index = 0;
                self.overlay = if self.overlay == Some(Overlay::CommandPalette) {
                    None
                } else {
                    Some(Overlay::CommandPalette)
                };
                Vec::new()
            }
            AppEvent::OpenSearch => {
                self.overlay = Some(Overlay::Search);
                Vec::new()
            }
            AppEvent::Escape => {
                self.disarm_session_delete();
                self.overlay = None;
                self.search.clear();
                Vec::new()
            }
            AppEvent::Scroll(delta) => {
                if self.focus == Focus::Sessions {
                    self.disarm_session_delete();
                    let last = self.sessions.len().saturating_sub(1);
                    self.session_index = if delta.is_negative() {
                        self.session_index
                            .saturating_sub(delta.unsigned_abs() as usize)
                    } else {
                        self.session_index.saturating_add(delta as usize).min(last)
                    };
                    return Vec::new();
                }
                if self.focus == Focus::Tools {
                    self.tool_panel.select(delta, self.tools.len());
                    return Vec::new();
                }
                self.scroll = if delta.is_negative() {
                    self.scroll
                        .saturating_add(delta.unsigned_abs())
                        .min(self.scroll_max)
                } else {
                    self.scroll.saturating_sub(delta as u16)
                };
                Vec::new()
            }
            AppEvent::SwitchLanguage(language) => {
                self.language = language;
                self.overlay = None;
                vec![Effect::PersistLanguage(language)]
            }
            AppEvent::NewSession => {
                if self.is_busy() {
                    self.error = Some(
                        "Wait for the current response or cancel it first / 请先等待或取消当前回复"
                            .into(),
                    );
                    return Vec::new();
                }
                vec![Effect::Backend(BackendCommand::NewSession)]
            }
            AppEvent::Exit(action) => {
                self.should_exit = true;
                self.exit_action = action;
                vec![Effect::Backend(BackendCommand::Shutdown)]
            }
        }
    }

    pub fn restore_parked_draft(&mut self) {
        if self.composer.is_empty()
            && let Some(draft) = self.parked_draft.take()
        {
            self.composer.replace(&draft);
            self.refresh_slash_completion();
        }
    }

    pub fn recall_history(&mut self, delta: i32) {
        if delta < 0 {
            let index = match self.history_index {
                None if !self.history.is_empty() => {
                    self.history_draft = self.composer.as_str().to_string();
                    self.history.len() - 1
                }
                Some(index) if !self.history.is_empty() => {
                    index.saturating_sub(1).min(self.history.len() - 1)
                }
                _ => return,
            };
            self.history_index = Some(index);
            self.composer.replace(&self.history[index]);
        } else if let Some(index) = self.history_index {
            if index + 1 < self.history.len() {
                self.history_index = Some(index + 1);
                self.composer.replace(&self.history[index + 1]);
            } else {
                self.history_index = None;
                self.composer.replace(&self.history_draft);
            }
        }
        self.refresh_slash_completion();
    }

    pub fn begin_skill(&mut self, name: String, args: Option<String>) -> Vec<Effect> {
        if let Some(args) = &args
            && args.contains('@')
        {
            return vec![Effect::PrepareSkill {
                name,
                args: args.clone(),
            }];
        }
        self.begin_submit(BackendCommand::InvokeSkill {
            name,
            args,
            context: None,
        })
    }

    pub fn begin_submit(&mut self, command: BackendCommand) -> Vec<Effect> {
        let text = self.composer.clear();
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
            if self.history.len() > 100 {
                self.history.remove(0);
            }
        }
        self.history_index = None;
        self.history_draft.clear();
        self.screen = Screen::Main;
        self.scroll = 0;
        self.close_reasoning();
        self.turn_message_start = self.messages.len();
        self.generation_phase = GenerationPhase::Waiting;
        self.animation_frame = 0;
        self.messages.push(Message {
            role: MessageRole::User,
            content: text,
            expanded: true,
        });
        self.messages.push(Message {
            role: MessageRole::Assistant,
            content: String::new(),
            expanded: true,
        });
        self.responding = true;
        self.error = None;
        self.elapsed_seconds = 0;
        self.response_started = Some(Instant::now());
        vec![Effect::Backend(command)]
    }

    fn submit(&mut self) -> Vec<Effect> {
        if self.is_busy() || self.composer.is_empty() {
            return Vec::new();
        }
        let text = self.composer.as_str().to_string();
        if text.trim().is_empty() {
            return Vec::new();
        }
        if text.contains('@') {
            return vec![Effect::PrepareSubmit(text)];
        }
        self.begin_submit(BackendCommand::Submit(text))
    }

    fn apply_backend(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::Skills(response) => {
                self.skills = response.skills;
                self.skills_loading = false;
                self.skills_error = None;
            }
            BackendEvent::SkillsError(error) => {
                self.skills_loading = false;
                self.skills_error = Some(error);
            }
            BackendEvent::Workflows(response) => {
                self.workflows = response.workflows;
                self.screen = Screen::Main;
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: workflow_list_message(self.language, &self.workflows),
                    expanded: true,
                });
                self.scroll = 0;
            }
            BackendEvent::WorkflowsError(error) => {
                self.error = Some(error);
            }
            BackendEvent::SessionReady(id) => {
                self.session_id = id;
            }
            BackendEvent::NewSessionCreated(id) => {
                self.session_id = id;
                self.messages.clear();
                self.reasoning_segments.clear();
                self.active_reasoning = None;
                self.reveal_reasoning = None;
                self.turn_message_start = 0;
                self.generation_phase = GenerationPhase::Idle;
                self.elapsed_seconds = 0;
                self.tools.clear();
                self.tool_panel = ToolPanelState::default();
                self.composer.clear();
                self.overlay = None;
                self.scroll = 0;
                self.focus = Focus::Chat;
                self.screen = Screen::Welcome;
                self.error = None;
                self.compaction = CompactionUiState::Idle;
                self.context_usage = None;
                self.recovery_suppression = "none".into();
                self.select_current_session();
            }
            BackendEvent::Resumed(id) => {
                self.session_id = id;
                self.select_current_session();
                self.messages.clear();
                self.reasoning_segments.clear();
                self.active_reasoning = None;
                self.reveal_reasoning = None;
                self.turn_message_start = 0;
                self.generation_phase = GenerationPhase::Idle;
                self.elapsed_seconds = 0;
                self.tools.clear();
                self.tool_panel = ToolPanelState::default();
                self.scroll = 0;
                self.focus = Focus::Chat;
                self.screen = Screen::Main;
                self.compaction = CompactionUiState::Idle;
                self.context_usage = None;
                self.recovery_suppression = "none".into();
            }
            BackendEvent::Sessions(summaries) => self.replace_sessions(summaries),
            BackendEvent::SessionRenamed(summary) => {
                let title = summary.title.clone();
                if let Some(session) = self
                    .sessions
                    .iter_mut()
                    .find(|session| session.id == summary.session_id)
                {
                    *session = SessionItem::from(summary);
                }
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: match self.language {
                        Language::ZhCn => format!("会话已重命名为“{title}”"),
                        Language::En => format!("Session renamed to \"{title}\""),
                    },
                    expanded: true,
                });
                self.error = None;
            }
            BackendEvent::SessionDeleted {
                session_id,
                replacement_session_id,
                sessions,
            } => {
                if let Some(replacement) = replacement_session_id {
                    self.session_id = replacement;
                    self.messages.clear();
                    self.reasoning_segments.clear();
                    self.active_reasoning = None;
                    self.reveal_reasoning = None;
                    self.turn_message_start = 0;
                    self.generation_phase = GenerationPhase::Idle;
                    self.elapsed_seconds = 0;
                    self.tools.clear();
                    self.tool_panel = ToolPanelState::default();
                    self.screen = Screen::Welcome;
                }
                self.replace_sessions(sessions);
                self.error = None;
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: format!("Deleted session {session_id} permanently."),
                    expanded: true,
                });
            }
            BackendEvent::Update(update) => self.apply_update(update),
            BackendEvent::ModelSwitched(_) => {
                unreachable!("model switch acknowledgements are reduced before generic updates")
            }
            BackendEvent::TurnCompleted(text) => {
                if !text.is_empty()
                    && !self.messages[self.turn_message_start.min(self.messages.len())..]
                        .iter()
                        .any(|message| {
                            message.role == MessageRole::Assistant && !message.content.is_empty()
                        })
                {
                    self.apply_update(ClientUpdate::TextDelta(text));
                }
                self.generation_phase = GenerationPhase::Completed;
                self.finish_turn();
            }
            BackendEvent::TurnCancelled => {
                self.generation_phase = GenerationPhase::Cancelled;
                self.error = Some("cancelled".into());
                self.finish_turn();
            }
            BackendEvent::Error(error) => {
                if self.responding {
                    self.generation_phase = GenerationPhase::Failed;
                }
                self.error = Some(error);
                self.finish_turn();
            }
        }
    }

    fn apply_update(&mut self, update: ClientUpdate) {
        match update {
            ClientUpdate::TextDelta(text) => {
                if text.is_empty() {
                    return;
                }
                self.close_reasoning();
                self.generation_phase = GenerationPhase::Answering;
                if let Some(message) = self.messages.last_mut()
                    && message.role == MessageRole::Assistant
                {
                    message.content.push_str(&text);
                } else {
                    self.messages.push(Message {
                        role: MessageRole::Assistant,
                        content: text,
                        expanded: true,
                    });
                }
            }
            ClientUpdate::ReasoningDelta(text) => {
                if text.is_empty() {
                    return;
                }
                self.generation_phase = GenerationPhase::Thinking;
                if let Some(index) = self.active_reasoning {
                    self.messages[index].content.push_str(&text);
                } else {
                    if self.messages.last().is_some_and(|message| {
                        message.role == MessageRole::Assistant && message.content.is_empty()
                    }) {
                        self.messages.pop();
                    }
                    let index = self.messages.len();
                    self.messages.push(Message {
                        role: MessageRole::Reasoning,
                        content: text,
                        expanded: true,
                    });
                    self.reasoning_segments.push(ReasoningSegment {
                        message_index: index,
                        started_at: Instant::now(),
                        duration: None,
                    });
                    self.active_reasoning = Some(index);
                }
            }
            ClientUpdate::ToolStarted {
                id,
                name,
                arguments,
            } => {
                self.close_reasoning();
                self.generation_phase = GenerationPhase::Tools;
                self.tools.push(ToolCard {
                    expanded: false,
                    id,
                    name,
                    arguments,
                    status: ToolStatus::Running,
                    result: None,
                    started_at: Instant::now(),
                    elapsed_ms: 0,
                });
                if self.focus != Focus::Tools {
                    self.tool_panel.select_last(self.tools.len());
                }
            }
            ClientUpdate::ToolFinished { id, result } => {
                self.finish_tool(&id, ToolStatus::Done, result)
            }
            ClientUpdate::ToolFailed { id, error } => {
                self.finish_tool(&id, ToolStatus::Error, error)
            }
            ClientUpdate::CompactionStarted { trigger, .. } => {
                self.compaction = CompactionUiState::Running {
                    started_at: Instant::now(),
                    trigger,
                };
                self.error = None;
            }
            ClientUpdate::CompactionCompleted {
                before,
                after,
                checkpoint_id,
                warning,
            } => {
                let content = match self.language {
                    Language::ZhCn => format!(
                        "上下文压缩完成：{} 条消息 → {} 条消息{}",
                        before.message_count,
                        after.message_count,
                        if warning.is_some() {
                            "。已从提交的检查点恢复派生历史。"
                        } else {
                            ""
                        }
                    ),
                    Language::En => format!(
                        "Context compacted: {} messages → {} messages{}",
                        before.message_count,
                        after.message_count,
                        if warning.is_some() {
                            ". Derived history was recovered from the committed checkpoint."
                        } else {
                            ""
                        }
                    ),
                };
                self.messages.push(Message {
                    role: MessageRole::System,
                    content,
                    expanded: true,
                });
                self.compaction = CompactionUiState::Completed {
                    before,
                    after,
                    checkpoint_id,
                    warning,
                };
            }
            ClientUpdate::CompactionFailed { code, message } => {
                let content = match self.language {
                    Language::ZhCn => format!("上下文压缩失败，原历史仍然有效：{message}"),
                    Language::En => {
                        format!(
                            "Context compaction failed; previous history remains active: {message}"
                        )
                    }
                };
                self.messages.push(Message {
                    role: MessageRole::System,
                    content,
                    expanded: true,
                });
                self.error = Some(message.clone());
                self.compaction = CompactionUiState::Failed { code, message };
            }
            ClientUpdate::CompactionCancelled => {
                self.compaction = CompactionUiState::Cancelled;
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: match self.language {
                        Language::ZhCn => "上下文压缩已取消。".into(),
                        Language::En => "Context compaction cancelled.".into(),
                    },
                    expanded: true,
                });
            }
            ClientUpdate::ContextUsage {
                estimated_input_tokens,
                context_window,
                utilization_percent,
            } => {
                self.context_usage = Some(ContextUiState {
                    estimated_input_tokens,
                    context_window,
                    utilization_percent,
                });
            }
            ClientUpdate::RecoverySuppression(suppression) => {
                self.recovery_suppression = suppression;
            }
            ClientUpdate::ModelChanged {
                provider,
                model,
                warning,
            } => {
                let selection = format!("{provider}/{model}");
                if self.model != selection {
                    self.context_usage = None;
                }
                self.model = selection;
                self.error = warning.map(|warning| warning.message);
            }
            ClientUpdate::WorkflowRun(run) => {
                self.apply_workflow_run(run);
            }
            ClientUpdate::PermissionRequested | ClientUpdate::Unknown => {}
        }
    }

    /// Upsert a live workflow run snapshot delivered via `session/update`
    /// (`sessionUpdate: "lato/workflow"`, Phase 7B4).
    fn apply_workflow_run(&mut self, run: crate::client::WorkflowRunView) {
        if let Some(existing) = self
            .workflow_runs
            .iter_mut()
            .find(|state| state.run_id == run.run_id)
        {
            *existing = run;
        } else {
            self.workflow_runs.push(run);
        }
        self.workflow_runs
            .sort_by(|left, right| left.display_name.cmp(&right.display_name));
    }

    fn finish_tool(&mut self, id: &str, status: ToolStatus, result: String) {
        if let Some(tool) = self.tools.iter_mut().rev().find(|tool| tool.id == id) {
            tool.status = status;
            tool.result = Some(result);
            tool.elapsed_ms = tool.started_at.elapsed().as_millis();
        }
    }

    pub fn toggle_reasoning(&mut self) {
        let expand = self
            .messages
            .iter()
            .any(|message| message.role == MessageRole::Reasoning && !message.expanded);
        for message in &mut self.messages {
            if message.role == MessageRole::Reasoning {
                message.expanded = expand;
            }
        }
        if expand {
            self.reveal_reasoning = self
                .messages
                .iter()
                .rposition(|message| message.role == MessageRole::Reasoning);
        }
    }

    fn close_reasoning(&mut self) {
        if let Some(index) = self.active_reasoning.take() {
            if let Some(message) = self.messages.get_mut(index) {
                message.expanded = false;
            }
            if let Some(segment) = self
                .reasoning_segments
                .iter_mut()
                .rev()
                .find(|segment| segment.message_index == index)
            {
                segment.duration = Some(segment.started_at.elapsed());
            }
        }
    }

    fn finish_turn(&mut self) {
        self.close_reasoning();
        if let Some(started) = self.response_started {
            self.elapsed_seconds = started.elapsed().as_secs();
        }
        self.responding = false;
        self.response_started = None;
        for tool in &mut self.tools {
            if tool.status == ToolStatus::Running {
                tool.status = ToolStatus::Done;
            }
        }
    }

    pub fn is_busy(&self) -> bool {
        self.responding || matches!(&self.compaction, CompactionUiState::Running { .. })
    }

    pub fn compaction_status(&self) -> &'static str {
        match &self.compaction {
            CompactionUiState::Idle => "idle",
            CompactionUiState::Running { .. } => "running",
            CompactionUiState::Completed { .. } => "completed",
            CompactionUiState::Failed { .. } => "failed",
            CompactionUiState::Cancelled => "cancelled",
        }
    }
}

fn workflow_list_message(language: Language, workflows: &[crate::client::WorkflowEntry]) -> String {
    if workflows.is_empty() {
        return match language {
            Language::ZhCn => "没有可用工作流。使用 --plugin-dir 加载受信任插件。".into(),
            Language::En => {
                "No materialized workflows. Use --plugin-dir with a trusted plugin.".into()
            }
        };
    }
    let mut lines = match language {
        Language::ZhCn => vec!["可用工作流：".to_string()],
        Language::En => vec!["Available workflows:".to_string()],
    };
    for workflow in workflows {
        let description = if workflow.description.is_empty() {
            "-"
        } else {
            workflow.description.as_str()
        };
        lines.push(format!(
            "{}\t{}\t{}{}",
            workflow.id,
            description,
            workflow.source,
            if workflow.compiled { "\tcompiled" } else { "" }
        ));
    }
    lines.join("\n")
}

fn compact_session_title(id: &str) -> String {
    if id.chars().count() > 18 {
        format!("{}…", id.chars().take(17).collect::<String>())
    } else {
        id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> AppState {
        AppState::new(
            Language::En,
            PathBuf::from("/tmp/lato"),
            "test/model".into(),
            "session-1".into(),
            vec![],
        )
    }

    #[test]
    fn responsive_modes_degrade_in_order() {
        assert_eq!(LayoutMode::for_size(120, 30), LayoutMode::Wide);
        assert_eq!(LayoutMode::for_size(82, 30), LayoutMode::Medium);
        assert_eq!(LayoutMode::for_size(58, 30), LayoutMode::Narrow);
        assert_eq!(LayoutMode::for_size(39, 12), LayoutMode::TooSmall);
    }

    #[test]
    fn submit_creates_stream_target_and_effect() {
        let mut app = app();
        app.composer.insert_str("hello");
        let effects = app.reduce(AppEvent::Submit);
        assert!(app.responding);
        assert_eq!(app.screen, Screen::Main);
        assert_eq!(app.messages.len(), 2);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Backend(BackendCommand::Submit(text))] if text == "hello"
        ));
    }

    #[test]
    fn compaction_updates_preserve_composer_and_render_one_terminal_system_message() {
        let mut app = app();
        app.composer.insert_str("draft next prompt");
        app.apply_update(ClientUpdate::CompactionStarted {
            compaction_id: "compact-1".into(),
            trigger: "manual".into(),
        });
        assert!(app.is_busy());
        assert!(!app.responding);
        assert_eq!(app.composer.as_str(), "draft next prompt");

        app.apply_update(ClientUpdate::CompactionCompleted {
            before: CompactionSizeResponse {
                message_count: 12,
                serialized_bytes: 12_000,
            },
            after: CompactionSizeResponse {
                message_count: 3,
                serialized_bytes: 2_000,
            },
            checkpoint_id: "cp-1".into(),
            warning: None,
        });
        assert!(!app.is_busy());
        assert_eq!(app.composer.as_str(), "draft next prompt");
        assert!(matches!(
            app.compaction,
            CompactionUiState::Completed { .. }
        ));
        assert_eq!(app.messages.len(), 1);
        assert!(app.messages[0].content.contains("12 messages → 3 messages"));
    }

    #[test]
    fn model_changes_only_after_backend_acknowledgement() {
        let mut app = app();
        let original = app.model.clone();
        assert_eq!(app.model, original);

        let effects = app.reduce(AppEvent::Backend(BackendEvent::ModelSwitched(
            crate::client::ModelSwitchResponse {
                supported: true,
                provider: "fixture".into(),
                model: "small-b".into(),
                compaction_warning: None,
            },
        )));

        assert_eq!(app.model, "fixture/small-b");
        assert!(matches!(
            effects.as_slice(),
            [Effect::PersistModel(selection)] if selection == "fixture/small-b"
        ));
    }

    #[test]
    fn new_session_acknowledgement_resets_to_welcome() {
        let mut app = app();
        app.screen = Screen::Main;
        app.messages.push(Message {
            role: MessageRole::User,
            content: "old conversation".into(),
            expanded: true,
        });
        app.apply_update(ClientUpdate::ToolStarted {
            id: "tool-1".into(),
            name: "read".into(),
            arguments: "{}".into(),
        });
        app.scroll = 4;
        app.focus = Focus::Tools;
        app.error = Some("old error".into());

        app.apply_backend(BackendEvent::NewSessionCreated("session-2".into()));

        assert_eq!(app.session_id, "session-2");
        assert_eq!(app.screen, Screen::Welcome);
        assert!(app.messages.is_empty());
        assert!(app.tools.is_empty());
        assert_eq!(app.scroll, 0);
        assert_eq!(app.focus, Focus::Chat);
        assert!(app.error.is_none());
    }

    #[test]
    fn tool_inspection_survives_updates_and_resets_with_conversation() {
        let mut app = app();
        app.apply_update(ClientUpdate::ToolStarted {
            id: "1".into(),
            name: "read".into(),
            arguments: "{}".into(),
        });
        app.tools[0].expanded = true;
        app.focus = Focus::Tools;
        app.apply_update(ClientUpdate::ToolStarted {
            id: "2".into(),
            name: "read".into(),
            arguments: "{}".into(),
        });
        app.apply_update(ClientUpdate::ToolFailed {
            id: "1".into(),
            error: "failed".into(),
        });
        assert!(app.tools[0].expanded);
        assert_eq!(app.tools[0].status, ToolStatus::Error);
        assert_eq!(app.tool_panel.selected, 0);
        app.focus = Focus::Chat;
        app.apply_update(ClientUpdate::ToolStarted {
            id: "3".into(),
            name: "read".into(),
            arguments: "{}".into(),
        });
        assert_eq!(app.tool_panel.selected, 2);
        app.tool_panel.scroll = 10;
        app.reduce(AppEvent::NewSession);
        app.apply_backend(BackendEvent::NewSessionCreated("session-2".into()));
        assert_eq!(app.tool_panel.selected, 0);
        assert_eq!(app.tool_panel.scroll, 0);
        app.apply_update(ClientUpdate::ToolStarted {
            id: "4".into(),
            name: "read".into(),
            arguments: "{}".into(),
        });
        app.tool_panel.scroll = 5;
        app.apply_backend(BackendEvent::Resumed("other".into()));
        assert!(app.tools.is_empty());
        assert_eq!(app.tool_panel.scroll, 0);
    }

    #[test]
    fn tool_lifecycle_updates_one_card() {
        let mut app = app();
        app.apply_update(ClientUpdate::ToolStarted {
            id: "1".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        });
        app.apply_update(ClientUpdate::ToolFinished {
            id: "1".into(),
            result: "ok".into(),
        });
        assert_eq!(app.tools.len(), 1);
        assert_eq!(app.tools[0].status, ToolStatus::Done);
        assert_eq!(app.tools[0].result.as_deref(), Some("ok"));
    }

    #[test]
    fn delete_confirmation_is_bound_to_the_selected_session() {
        let mut app = app();
        app.sessions = vec![
            SessionItem {
                id: "session-1".into(),
                title: "First".into(),
                timestamp: String::new(),
            },
            SessionItem {
                id: "session-2".into(),
                title: "Second".into(),
                timestamp: String::new(),
            },
        ];
        app.focus = Focus::Sessions;
        assert_eq!(app.arm_or_confirm_selected_delete(), None);
        app.reduce(AppEvent::Scroll(1));
        assert!(app.armed_session_delete.is_none());
        assert_eq!(app.arm_or_confirm_selected_delete(), None);
        assert_eq!(
            app.arm_or_confirm_selected_delete(),
            Some("session-2".into())
        );
    }

    #[test]
    fn structured_sessions_keep_titles_and_select_the_current_session() {
        let app = AppState::new_with_summaries(
            Language::En,
            PathBuf::from("/tmp/lato"),
            "test/model".into(),
            "session-2".into(),
            vec![
                SessionSummary {
                    session_id: "session-1".into(),
                    title: "First title".into(),
                    title_source: "automatic".into(),
                    created_at_ms: 1,
                    updated_at_ms: 2,
                },
                SessionSummary {
                    session_id: "session-2".into(),
                    title: "Manual title".into(),
                    title_source: "manual".into(),
                    created_at_ms: 1,
                    updated_at_ms: 3,
                },
            ],
        );
        assert_eq!(app.sessions[1].title, "Manual title");
        assert_eq!(app.session_index, 1);
    }

    #[test]
    fn rename_acknowledgement_updates_title_and_preserves_conversation() {
        let mut app = AppState::new_with_summaries(
            Language::En,
            PathBuf::from("/tmp/lato"),
            "test/model".into(),
            "session-1".into(),
            vec![SessionSummary {
                session_id: "session-1".into(),
                title: "Old title".into(),
                title_source: "automatic".into(),
                created_at_ms: 1,
                updated_at_ms: 2,
            }],
        );
        app.screen = Screen::Main;
        app.messages.push(Message {
            role: MessageRole::User,
            content: "keep me".into(),
            expanded: true,
        });

        app.apply_backend(BackendEvent::SessionRenamed(SessionSummary {
            session_id: "session-1".into(),
            title: "New Title".into(),
            title_source: "manual".into(),
            created_at_ms: 1,
            updated_at_ms: 3,
        }));

        assert_eq!(app.sessions[0].title, "New Title");
        assert_eq!(app.screen, Screen::Main);
        assert_eq!(app.messages[0].content, "keep me");
        assert!(matches!(
            app.messages.last(),
            Some(Message {
                role: MessageRole::System,
                content,
                ..
            }) if content == "Session renamed to \"New Title\""
        ));
    }

    #[test]
    fn recovery_suppression_updates_status_without_adding_transcript_items() {
        let mut app = AppState::new(
            Language::En,
            PathBuf::from("/tmp/lato"),
            "test/model".into(),
            "session-1".into(),
            Vec::new(),
        );
        for mode in ["turn", "sticky", "until_success", "auth", "none"] {
            let before = app.messages.len();
            app.apply_update(ClientUpdate::RecoverySuppression(mode.into()));
            assert_eq!(app.recovery_suppression, mode);
            assert_eq!(app.messages.len(), before);
        }
    }
}
