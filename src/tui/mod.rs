pub mod backend;
pub mod event;
pub mod i18n;
pub mod input;
pub mod render;
pub mod state;
pub mod terminal;
pub mod widgets;

use self::{
    backend::{ApprovalPrompt, BackendCommand, BackendHandle},
    i18n::{Language, TextKey, tr},
    state::{AppEvent, AppState, ApprovalState, Effect, Message, MessageRole, Overlay},
    terminal::TerminalGuard,
};
use crate::client::InteractiveAcpClient;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use lato_workspace::SessionTrust;
use std::path::PathBuf;
use tokio::sync::mpsc;

pub struct InteractiveBootstrap {
    pub client: InteractiveAcpClient,
    pub approvals: mpsc::UnboundedReceiver<ApprovalPrompt>,
    pub trust: SessionTrust,
    pub language: Language,
    pub workspace: PathBuf,
    pub model: String,
    pub home: PathBuf,
    pub sessions: Vec<String>,
}

pub async fn run(bootstrap: InteractiveBootstrap) -> Result<(), String> {
    let (_guard, terminal) = TerminalGuard::enter()?;
    tokio::task::LocalSet::new()
        .run_until(run_loop(terminal, bootstrap))
        .await
}

async fn run_loop(
    mut terminal: terminal::TuiTerminal,
    mut bootstrap: InteractiveBootstrap,
) -> Result<(), String> {
    let session_id = bootstrap.client.session_id().to_string();
    let (backend, mut backend_events) = backend::spawn(bootstrap.client);
    let mut app = AppState::new(
        bootstrap.language,
        bootstrap.workspace,
        bootstrap.model,
        session_id,
        bootstrap.sessions,
    );
    let size = terminal
        .size()
        .map_err(|error| format!("read terminal size: {error}"))?;
    app.reduce(AppEvent::Resize(size.width, size.height));
    let mut terminal_events = EventStream::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut approvals_open = true;

    terminal
        .draw(|frame| render::render(frame, &app))
        .map_err(|error| format!("draw TUI: {error}"))?;

    while !app.should_exit {
        let (effects, needs_draw) = tokio::select! {
            event = terminal_events.next() => match event {
                Some(Ok(event)) => (handle_terminal_event(&mut app, event, &backend, &bootstrap.trust), true),
                Some(Err(error)) => {
                    app.error = Some(error.to_string());
                    (Vec::new(), true)
                }
                None => (app.reduce(AppEvent::Exit), true),
            },
            event = backend_events.recv() => match event {
                Some(event) => (app.reduce(AppEvent::Backend(event)), true),
                None => {
                    app.error = Some("backend event channel closed".into());
                    (app.reduce(AppEvent::Exit), true)
                }
            },
            approval = bootstrap.approvals.recv(), if approvals_open => {
                if let Some(approval) = approval {
                    app.approval = Some(ApprovalState {
                        tool: approval.tool,
                        summary: approval.summary,
                        response: approval.response,
                    });
                    (Vec::new(), true)
                } else {
                    approvals_open = false;
                    (Vec::new(), false)
                }
            },
            _ = tick.tick() => {
                let animating = app.responding
                    || app.tools.iter().any(|tool| tool.status == state::ToolStatus::Running);
                (app.reduce(AppEvent::Tick), animating)
            },
        };
        execute_effects(effects, &backend, &bootstrap.home, &mut app);
        if needs_draw {
            terminal
                .draw(|frame| render::render(frame, &app))
                .map_err(|error| format!("draw TUI: {error}"))?;
        }
    }
    let _ = backend.send(BackendCommand::Shutdown);
    Ok(())
}

fn execute_effects(
    effects: Vec<Effect>,
    backend: &BackendHandle,
    home: &std::path::Path,
    app: &mut AppState,
) {
    for effect in effects {
        match effect {
            Effect::Backend(command) => {
                if let Err(error) = backend.send(command) {
                    app.error = Some(error);
                }
            }
            Effect::PersistLanguage(language) => {
                if let Err(error) = crate::cli::persist_language(home, language) {
                    app.error = Some(error);
                }
            }
        }
    }
}

fn handle_terminal_event(
    app: &mut AppState,
    event: Event,
    backend: &BackendHandle,
    trust: &SessionTrust,
) -> Vec<Effect> {
    match event {
        Event::Resize(width, height) => app.reduce(AppEvent::Resize(width, height)),
        Event::Paste(text) => {
            active_input(app).insert_str(&text);
            Vec::new()
        }
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            handle_key(app, key, backend, trust)
        }
        _ => Vec::new(),
    }
}

fn handle_key(
    app: &mut AppState,
    key: KeyEvent,
    backend: &BackendHandle,
    trust: &SessionTrust,
) -> Vec<Effect> {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if app.responding {
            if let Err(error) = backend.send(BackendCommand::Cancel) {
                app.error = Some(error);
            }
            return Vec::new();
        }
        return app.reduce(AppEvent::Exit);
    }
    if app.approval.is_some() {
        return handle_approval_key(app, key);
    }
    if app.overlay == Some(Overlay::CommandPalette) {
        return handle_palette_key(app, key, trust);
    }
    if app.overlay == Some(Overlay::Search) {
        return handle_search_key(app, key);
    }

    if key.code == KeyCode::Char('k')
        && key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
    {
        return app.reduce(AppEvent::TogglePalette);
    }
    match key.code {
        KeyCode::Tab => app.reduce(AppEvent::FocusNext),
        KeyCode::BackTab => app.reduce(AppEvent::FocusPrevious),
        KeyCode::Esc => app.reduce(AppEvent::Escape),
        KeyCode::Enter => submit_or_command(app, trust),
        KeyCode::Backspace => {
            app.composer.backspace();
            Vec::new()
        }
        KeyCode::Delete => {
            app.composer.delete();
            Vec::new()
        }
        KeyCode::Left => {
            app.composer.move_left();
            Vec::new()
        }
        KeyCode::Right => {
            app.composer.move_right();
            Vec::new()
        }
        KeyCode::Home => {
            app.composer.move_home();
            Vec::new()
        }
        KeyCode::End => {
            app.composer.move_end();
            Vec::new()
        }
        KeyCode::Up => app.reduce(AppEvent::Scroll(-1)),
        KeyCode::Down => app.reduce(AppEvent::Scroll(1)),
        KeyCode::Char('/') if app.composer.is_empty() && app.screen == state::Screen::Main => {
            app.reduce(AppEvent::OpenSearch)
        }
        KeyCode::Char('j') if app.focus != state::Focus::Chat => app.reduce(AppEvent::Scroll(1)),
        KeyCode::Char('k') if app.focus != state::Focus::Chat => app.reduce(AppEvent::Scroll(-1)),
        KeyCode::Char(value)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::ALT) =>
        {
            app.composer.insert_char(value);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn submit_or_command(app: &mut AppState, trust: &SessionTrust) -> Vec<Effect> {
    let command = app.composer.as_str().trim().to_ascii_lowercase();
    if !command.starts_with('/') {
        return app.reduce(AppEvent::Submit);
    }
    match command.as_str() {
        "/exit" | "/quit" => {
            app.composer.clear();
            app.reduce(AppEvent::Exit)
        }
        "/clear" => {
            app.composer.clear();
            app.reduce(AppEvent::ClearConversation)
        }
        "/lang" | "/language" => {
            app.composer.clear();
            let language = match app.language {
                Language::ZhCn => Language::En,
                Language::En => Language::ZhCn,
            };
            app.reduce(AppEvent::SwitchLanguage(language))
        }
        "/approve" => {
            app.composer.clear();
            trust.allow_once();
            Vec::new()
        }
        "/status" => {
            app.composer.clear();
            app.screen = state::Screen::Main;
            app.messages.push(Message {
                role: MessageRole::System,
                content: format!("{} · {}", app.model, app.workspace.display()),
                expanded: true,
            });
            Vec::new()
        }
        "/help" => {
            app.composer.clear();
            app.screen = state::Screen::Main;
            app.messages.push(Message {
                role: MessageRole::System,
                content: "/help  /clear  /lang  /approve  /status  /exit".into(),
                expanded: true,
            });
            Vec::new()
        }
        _ => {
            app.error = Some(format!("{}: {command}", tr(app.language, TextKey::Command)));
            app.composer.clear();
            Vec::new()
        }
    }
}

fn handle_approval_key(app: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let decision = match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(true),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
        _ => None,
    };
    if let Some(decision) = decision
        && let Some(approval) = app.approval.take()
    {
        let _ = approval.response.send(decision);
    }
    Vec::new()
}

fn handle_palette_key(app: &mut AppState, key: KeyEvent, trust: &SessionTrust) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc => app.reduce(AppEvent::Escape),
        KeyCode::Up => {
            app.palette_index = app.palette_index.saturating_sub(1);
            Vec::new()
        }
        KeyCode::Down => {
            app.palette_index = (app.palette_index + 1).min(event::COMMAND_COUNT - 1);
            Vec::new()
        }
        KeyCode::Enter => match app.palette_index {
            0 => app.reduce(AppEvent::ClearConversation),
            1 => {
                let language = match app.language {
                    Language::ZhCn => Language::En,
                    Language::En => Language::ZhCn,
                };
                app.reduce(AppEvent::SwitchLanguage(language))
            }
            2 => app.reduce(AppEvent::OpenSearch),
            3 => {
                trust.allow_once();
                app.overlay = None;
                Vec::new()
            }
            4 => {
                app.overlay = None;
                app.screen = state::Screen::Main;
                app.messages.push(Message {
                    role: MessageRole::System,
                    content: format!("{} · {}", app.model, app.workspace.display()),
                    expanded: true,
                });
                Vec::new()
            }
            _ => app.reduce(AppEvent::Exit),
        },
        _ => Vec::new(),
    }
}

fn handle_search_key(app: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => app.reduce(AppEvent::Escape),
        KeyCode::Backspace => {
            app.search.backspace();
            Vec::new()
        }
        KeyCode::Delete => {
            app.search.delete();
            Vec::new()
        }
        KeyCode::Left => {
            app.search.move_left();
            Vec::new()
        }
        KeyCode::Right => {
            app.search.move_right();
            Vec::new()
        }
        KeyCode::Char(value)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::ALT) =>
        {
            app.search.insert_char(value);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn active_input(app: &mut AppState) -> &mut input::InputBuffer {
    if app.overlay == Some(Overlay::Search) {
        &mut app.search
    } else {
        &mut app.composer
    }
}
