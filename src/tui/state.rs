use super::{
    backend::{BackendCommand, BackendEvent},
    i18n::Language,
    input::InputBuffer,
};
use crate::client::ClientUpdate;
use std::{path::PathBuf, time::Instant};

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

#[derive(Clone, Debug)]
pub struct ToolCard {
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

#[derive(Debug)]
pub struct ApprovalState {
    pub tool: String,
    pub summary: String,
    pub response: tokio::sync::oneshot::Sender<bool>,
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
    pub composer: InputBuffer,
    pub search: InputBuffer,
    pub focus: Focus,
    pub overlay: Option<Overlay>,
    pub palette_index: usize,
    pub approval: Option<ApprovalState>,
    pub layout: LayoutMode,
    pub responding: bool,
    pub response_started: Option<Instant>,
    pub elapsed_seconds: u64,
    pub error: Option<String>,
    pub scroll: u16,
    pub should_exit: bool,
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
    ClearConversation,
    Exit,
}

#[derive(Debug)]
pub enum Effect {
    Backend(BackendCommand),
    PersistLanguage(Language),
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
            composer: InputBuffer::new(),
            search: InputBuffer::new(),
            focus: Focus::Chat,
            overlay: None,
            palette_index: 0,
            approval: None,
            layout: LayoutMode::Wide,
            responding: false,
            response_started: None,
            elapsed_seconds: 0,
            error: None,
            scroll: 0,
            should_exit: false,
        }
    }

    pub fn reduce(&mut self, event: AppEvent) -> Vec<Effect> {
        match event {
            AppEvent::Submit => self.submit(),
            AppEvent::Backend(event) => {
                self.apply_backend(event);
                Vec::new()
            }
            AppEvent::Tick => {
                if let Some(started) = self.response_started {
                    self.elapsed_seconds = started.elapsed().as_secs();
                }
                for tool in &mut self.tools {
                    if tool.status == ToolStatus::Running {
                        tool.elapsed_ms = tool.started_at.elapsed().as_millis();
                    }
                }
                Vec::new()
            }
            AppEvent::Resize(width, height) => {
                self.layout = LayoutMode::for_size(width, height);
                Vec::new()
            }
            AppEvent::FocusNext => {
                self.focus = self.focus.next();
                Vec::new()
            }
            AppEvent::FocusPrevious => {
                self.focus = self.focus.previous();
                Vec::new()
            }
            AppEvent::TogglePalette => {
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
                self.overlay = None;
                self.search.clear();
                Vec::new()
            }
            AppEvent::Scroll(delta) => {
                self.scroll = if delta.is_negative() {
                    self.scroll.saturating_sub(delta.unsigned_abs())
                } else {
                    self.scroll.saturating_add(delta as u16)
                };
                Vec::new()
            }
            AppEvent::SwitchLanguage(language) => {
                self.language = language;
                self.overlay = None;
                vec![Effect::PersistLanguage(language)]
            }
            AppEvent::ClearConversation => {
                self.messages.clear();
                self.tools.clear();
                self.overlay = None;
                vec![Effect::Backend(BackendCommand::Clear)]
            }
            AppEvent::Exit => {
                self.should_exit = true;
                vec![Effect::Backend(BackendCommand::Shutdown)]
            }
        }
    }

    fn submit(&mut self) -> Vec<Effect> {
        if self.responding || self.composer.is_empty() {
            return Vec::new();
        }
        let text = self.composer.clear();
        self.screen = Screen::Main;
        self.messages.push(Message {
            role: MessageRole::User,
            content: text.clone(),
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
        vec![Effect::Backend(BackendCommand::Submit(text))]
    }

    fn apply_backend(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::SessionReady(id) | BackendEvent::Cleared(id) => {
                self.session_id = id;
            }
            BackendEvent::Update(update) => self.apply_update(update),
            BackendEvent::TurnCompleted(text) => {
                if let Some(message) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|message| message.role == MessageRole::Assistant)
                    && message.content.is_empty()
                {
                    message.content = text;
                }
                self.finish_turn();
            }
            BackendEvent::TurnCancelled => {
                self.error = Some("cancelled".into());
                self.finish_turn();
            }
            BackendEvent::Error(error) => {
                self.error = Some(error);
                self.finish_turn();
            }
        }
    }

    fn apply_update(&mut self, update: ClientUpdate) {
        match update {
            ClientUpdate::TextDelta(text) => {
                if let Some(message) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|message| message.role == MessageRole::Assistant)
                {
                    message.content.push_str(&text);
                }
            }
            ClientUpdate::ReasoningDelta(text) => {
                if let Some(message) = self.messages.last_mut()
                    && message.role == MessageRole::Reasoning
                {
                    message.content.push_str(&text);
                } else {
                    self.messages.push(Message {
                        role: MessageRole::Reasoning,
                        content: text,
                        expanded: false,
                    });
                }
            }
            ClientUpdate::ToolStarted {
                id,
                name,
                arguments,
            } => self.tools.push(ToolCard {
                id,
                name,
                arguments,
                status: ToolStatus::Running,
                result: None,
                started_at: Instant::now(),
                elapsed_ms: 0,
            }),
            ClientUpdate::ToolFinished { id, result } => {
                self.finish_tool(&id, ToolStatus::Done, result)
            }
            ClientUpdate::ToolFailed { id, error } => {
                self.finish_tool(&id, ToolStatus::Error, error)
            }
            ClientUpdate::PermissionRequested | ClientUpdate::Unknown => {}
        }
    }

    fn finish_tool(&mut self, id: &str, status: ToolStatus, result: String) {
        if let Some(tool) = self.tools.iter_mut().rev().find(|tool| tool.id == id) {
            tool.status = status;
            tool.result = Some(result);
            tool.elapsed_ms = tool.started_at.elapsed().as_millis();
        }
    }

    fn finish_turn(&mut self) {
        self.responding = false;
        self.response_started = None;
        for tool in &mut self.tools {
            if tool.status == ToolStatus::Running {
                tool.status = ToolStatus::Done;
            }
        }
    }
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
}
