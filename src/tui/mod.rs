pub mod backend;
mod commands;
pub mod dialog;
pub mod event;
pub mod i18n;
pub mod input;
pub mod render;
pub mod state;
pub mod terminal;
pub mod tool_panel;
pub mod widgets;

use self::{
    backend::{ApprovalPrompt, BackendCommand, BackendHandle},
    i18n::{Language, TextKey, tr},
    state::{AppEvent, AppState, ApprovalState, Effect, Message, MessageRole, Overlay},
};
use crate::client::{InteractiveAcpClient, SessionSummary};
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
    pub sessions: Vec<SessionSummary>,
    pub resumed: bool,
    pub switchable: std::sync::Arc<lato_ai::SwitchableModelStream>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum TuiExit {
    #[default]
    Quit,
}

pub async fn run(
    terminal: &mut terminal::TuiTerminal,
    terminal_events: &mut EventStream,
    mut bootstrap: InteractiveBootstrap,
) -> Result<TuiExit, String> {
    let session_id = bootstrap.client.session_id().to_string();
    let (backend, mut backend_events) = backend::spawn(bootstrap.client);
    let mut app = AppState::new_with_summaries(
        bootstrap.language,
        bootstrap.workspace,
        bootstrap.model,
        session_id,
        bootstrap.sessions,
    );
    app.messages.push(Message {
        role: MessageRole::System,
        content: crate::permissions::describe(&bootstrap.trust, bootstrap.language),
        expanded: true,
    });
    if bootstrap.resumed {
        app.screen = state::Screen::Main;
    }
    let size = terminal
        .size()
        .map_err(|error| format!("read terminal size: {error}"))?;
    app.reduce(AppEvent::Resize(size.width, size.height));
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut approvals_open = true;

    terminal
        .draw(|frame| render::render(frame, &mut app))
        .map_err(|error| format!("draw TUI: {error}"))?;

    while !app.should_exit {
        let (effects, needs_draw) = tokio::select! {
            event = terminal_events.next() => match event {
                Some(Ok(event)) => (handle_terminal_event(&mut app, event, &backend, &bootstrap.trust), true),
                Some(Err(error)) => {
                    app.error = Some(error.to_string());
                    (Vec::new(), true)
                }
                None => (app.reduce(AppEvent::Exit(TuiExit::Quit)), true),
            },
            event = backend_events.recv() => match event {
                Some(event) => (app.reduce(AppEvent::Backend(event)), true),
                None => {
                    app.error = Some("backend event channel closed".into());
                    (app.reduce(AppEvent::Exit(TuiExit::Quit)), true)
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
        execute_effects(
            effects,
            &backend,
            &bootstrap.home,
            &mut app,
            terminal,
            terminal_events,
            &bootstrap.switchable,
        )
        .await;
        if needs_draw {
            terminal
                .draw(|frame| render::render(frame, &mut app))
                .map_err(|error| format!("draw TUI: {error}"))?;
        }
    }
    let _ = backend.send(BackendCommand::Shutdown);
    Ok(app.exit_action)
}

async fn execute_effects(
    effects: Vec<Effect>,
    backend: &BackendHandle,
    home: &std::path::Path,
    app: &mut AppState,
    terminal: &mut terminal::TuiTerminal,
    events: &mut EventStream,
    switchable: &lato_ai::SwitchableModelStream,
) {
    for effect in effects {
        match effect {
            Effect::Backend(command) => {
                if let Err(error) = backend.send(command) {
                    app.error = Some(error);
                }
            }
            Effect::Doctor => {
                app.composer.clear();
                app.overlay = Some(Overlay::Configuration);
                let workspace = app.workspace.clone();
                let result = dialog::run(terminal, events, Some(app), |ui| async move {
                    ui.notice("Checking local configuration / 检查本地配置…");
                    Ok(crate::cli::doctor_report(home, &workspace).await)
                })
                .await;
                app.overlay = None;
                if let Ok(report) = result {
                    app.screen = state::Screen::Main;
                    app.messages.push(Message {
                        role: MessageRole::System,
                        content: report,
                        expanded: true,
                    });
                }
            }
            Effect::Sessions => {
                app.composer.clear();
                app.overlay = None;
                if app.responding {
                    app.error = Some(
                        "Cancel the current response before switching sessions / 请先取消当前回复"
                            .into(),
                    );
                    continue;
                }
                app.overlay = Some(Overlay::Configuration);
                let workspace = app.workspace.clone();
                let home = crate::cli::lato_home();
                let result = dialog::run(terminal, events, Some(app), |ui| async move {
                    let sessions =
                        crate::client::list_session_summaries_over_acp(workspace, home).await?;
                    let choices = sessions
                        .iter()
                        .map(|session| format!("{} · {}", session.title, session.session_id))
                        .collect::<Vec<_>>();
                    let selected = ui
                        .choose("Sessions / 会话 — type to filter", &choices)
                        .await?;
                    sessions
                        .into_iter()
                        .zip(choices)
                        .find_map(|(session, label)| {
                            (label == selected).then_some(session.session_id)
                        })
                        .ok_or_else(|| "selected session disappeared".to_string())
                })
                .await;
                app.overlay = None;
                match result {
                    Ok(id) if id != app.session_id => {
                        if let Err(error) = backend.send(BackendCommand::Resume(id)) {
                            app.error = Some(error);
                        }
                    }
                    Err(error) if error != dialog::CANCELLED => app.error = Some(error),
                    _ => {}
                }
            }
            Effect::ConfigureModel | Effect::Login => {
                if app.responding {
                    app.error = Some(
                        "Wait for the current response or cancel it first / 请先等待或取消当前回复"
                            .into(),
                    );
                    continue;
                }
                let login = matches!(effect, Effect::Login);
                app.overlay = Some(Overlay::Configuration);
                app.composer.clear();
                let current = app.model.clone();
                let result = dialog::run(terminal, events, Some(app), |ui| async move {
                    let selection = if login {
                        let (provider, _) = current
                            .split_once('/')
                            .ok_or("model must be provider/model")?;
                        crate::cli::configure_provider_auth(home, provider, true, &ui).await?;
                        current
                    } else {
                        crate::cli::configure_interactively(home, &ui).await?
                    };
                    ui.notice("Loading model / 加载模型…");
                    let stream = crate::cli::configured_stream(&selection).await?;
                    Ok((selection, stream))
                })
                .await;
                app.overlay = None;
                if let Ok(size) = terminal.size() {
                    app.reduce(AppEvent::Resize(size.width, size.height));
                }
                match result {
                    Ok((selection, stream)) => {
                        match crate::cli::persist_model_selection(home, &selection, app.language) {
                            Ok(()) => {
                                switchable.set(stream).await;
                                app.model = selection;
                                app.error = None;
                            }
                            Err(error) => app.error = Some(error),
                        }
                    }
                    Err(error) if error == dialog::CANCELLED => {}
                    Err(error) => app.error = Some(error),
                }
            }
            Effect::PersistLanguage(language) => {
                if let Err(error) = crate::cli::persist_language(home, language) {
                    app.error = Some(error);
                }
            }
            Effect::RenameSession { session_id, title } => {
                if app.responding {
                    app.error = Some(
                        "Wait for the current response or cancel it first / 请先等待或取消当前回复"
                            .into(),
                    );
                    continue;
                }
                app.overlay = Some(Overlay::Configuration);
                let prompt = match app.language {
                    Language::ZhCn => format!("重命名会话 {session_id}（当前：{title}）"),
                    Language::En => format!("Rename session {session_id} (current: {title})"),
                };
                let result = dialog::run(terminal, events, Some(app), |ui| async move {
                    ui.input_with_initial(prompt, title, false).await
                })
                .await;
                app.overlay = None;
                match result {
                    Ok(title) => {
                        let _ = backend.send(BackendCommand::RenameSession { session_id, title });
                    }
                    Err(error) if error == dialog::CANCELLED => {}
                    Err(error) => app.error = Some(error),
                }
            }
            Effect::ConfirmDeleteSession(session_id) => {
                if app.responding {
                    app.error = Some(
                        "Wait for the current response or cancel it first / 请先等待或取消当前回复"
                            .into(),
                    );
                    continue;
                }
                app.overlay = Some(Overlay::Configuration);
                let choices = match app.language {
                    Language::ZhCn => vec!["永久删除".to_string(), "取消".to_string()],
                    Language::En => vec!["Delete permanently".to_string(), "Cancel".to_string()],
                };
                let prompt = match app.language {
                    Language::ZhCn => format!("永久删除会话 {session_id}？此操作无法撤销。"),
                    Language::En => {
                        format!("Delete session {session_id} permanently? This cannot be undone.")
                    }
                };
                let result = dialog::run(terminal, events, Some(app), |ui| async move {
                    ui.choose(prompt, &choices).await
                })
                .await;
                app.overlay = None;
                match result {
                    Ok(answer) if answer == "永久删除" || answer == "Delete permanently" => {
                        let _ = backend.send(BackendCommand::DeleteSession(session_id));
                    }
                    Err(error) if error != dialog::CANCELLED => app.error = Some(error),
                    _ => {}
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
        Event::Paste(text) if app.approval.is_none() => {
            let composer_active = app.overlay != Some(Overlay::Search);
            active_input(app).insert_str(&text);
            if composer_active {
                app.refresh_slash_completion();
            }
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
        return app.reduce(AppEvent::Exit(TuiExit::Quit));
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
    if app.focus == state::Focus::Tools && tool_panel::handle_key(app, key) {
        return Vec::new();
    }
    if let Some(effects) = handle_slash_completion_key(app, key, trust) {
        return effects;
    }
    if app.armed_session_delete.is_some() && key.code != KeyCode::Char('d') {
        app.disarm_session_delete();
    }
    match key.code {
        KeyCode::Tab => app.reduce(AppEvent::FocusNext),
        KeyCode::BackTab => app.reduce(AppEvent::FocusPrevious),
        KeyCode::Esc => app.reduce(AppEvent::Escape),
        KeyCode::Enter if app.focus == state::Focus::Sessions => resume_selected_session(app),
        KeyCode::Char('r') if app.focus == state::Focus::Sessions => {
            app.disarm_session_delete();
            let Some(session) = app.sessions.get(app.session_index) else {
                return Vec::new();
            };
            vec![Effect::RenameSession {
                session_id: session.id.clone(),
                title: session.title.clone(),
            }]
        }
        KeyCode::Char('d') if app.focus == state::Focus::Sessions => app
            .arm_or_confirm_selected_delete()
            .map(|id| vec![Effect::Backend(BackendCommand::DeleteSession(id))])
            .unwrap_or_default(),
        KeyCode::Enter => submit_or_command(app, trust),
        KeyCode::Backspace => {
            app.composer.backspace();
            app.refresh_slash_completion();
            Vec::new()
        }
        KeyCode::Delete => {
            app.composer.delete();
            app.refresh_slash_completion();
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
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.reduce(AppEvent::OpenSearch)
        }
        KeyCode::Char('j') if app.focus != state::Focus::Chat => app.reduce(AppEvent::Scroll(1)),
        KeyCode::Char('k') if app.focus != state::Focus::Chat => app.reduce(AppEvent::Scroll(-1)),
        KeyCode::Char(value)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::ALT) =>
        {
            app.focus = state::Focus::Chat;
            app.composer.insert_char(value);
            app.refresh_slash_completion();
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn handle_slash_completion_key(
    app: &mut AppState,
    key: KeyEvent,
    trust: &SessionTrust,
) -> Option<Vec<Effect>> {
    let candidates = app.slash_completion();
    let selected = candidates.get(app.slash_completion_index).copied()?;
    match key.code {
        KeyCode::Up => {
            app.slash_completion_index = app.slash_completion_index.saturating_sub(1);
            Some(Vec::new())
        }
        KeyCode::Down => {
            app.slash_completion_index =
                (app.slash_completion_index + 1).min(candidates.len().saturating_sub(1));
            Some(Vec::new())
        }
        KeyCode::Esc => {
            app.dismiss_slash_completion();
            Some(Vec::new())
        }
        KeyCode::Enter => {
            if app.composer.as_str().eq_ignore_ascii_case(selected.name) {
                Some(submit_or_command(app, trust))
            } else {
                app.composer.replace(selected.name);
                app.refresh_slash_completion();
                Some(Vec::new())
            }
        }
        _ => None,
    }
}

fn submit_or_command(app: &mut AppState, trust: &SessionTrust) -> Vec<Effect> {
    let raw_command = app.composer.as_str().trim().to_string();
    let command = raw_command.to_ascii_lowercase();
    if !command.starts_with('/') {
        return app.reduce(AppEvent::Submit);
    }
    let name = command.split_whitespace().next().unwrap_or_default();
    match name {
        "/exit" | "/quit" => {
            app.composer.clear();
            app.reduce(AppEvent::Exit(TuiExit::Quit))
        }
        "/model" => vec![Effect::ConfigureModel],
        "/login" => vec![Effect::Login],
        "/doctor" => vec![Effect::Doctor],
        "/sessions" => vec![Effect::Sessions],
        "/rename" => {
            let title = raw_command
                .find(char::is_whitespace)
                .map(|index| raw_command[index..].trim())
                .unwrap_or_default();
            if title.is_empty() {
                let current_title = app
                    .sessions
                    .iter()
                    .find(|session| session.id == app.session_id)
                    .map(|session| session.title.clone())
                    .unwrap_or_else(|| app.session_id.clone());
                vec![Effect::RenameSession {
                    session_id: app.session_id.clone(),
                    title: current_title,
                }]
            } else {
                app.composer.clear();
                vec![Effect::Backend(BackendCommand::RenameSession {
                    session_id: app.session_id.clone(),
                    title: title.to_string(),
                })]
            }
        }
        "/delete" => {
            app.composer.clear();
            vec![Effect::ConfirmDeleteSession(app.session_id.clone())]
        }
        "/search" => {
            app.composer.clear();
            app.reduce(AppEvent::OpenSearch)
        }
        "/clear" | "/new" => {
            app.composer.clear();
            app.reduce(AppEvent::NewSession)
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
        "/status" | "/permissions" => {
            app.composer.clear();
            app.screen = state::Screen::Main;
            app.messages.push(Message {
                role: MessageRole::System,
                content: format!(
                    "{} · {}\n{}\n{}\nlato resume {} --sandbox off",
                    app.model,
                    app.workspace.display(),
                    crate::permissions::describe(trust, app.language),
                    match app.language {
                        Language::ZhCn => "如需更改范围，请退出并恢复会话。可选 off / workspace / read-only，例如：",
                        Language::En => "To change scope, exit and resume with off / workspace / read-only. Example:",
                    },
                    app.session_id
                ),
                expanded: true,
            });
            Vec::new()
        }
        "/help" => {
            app.composer.clear();
            app.screen = state::Screen::Main;
            app.messages.push(Message {
                role: MessageRole::System,
                content: commands::help_line(),
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

fn resume_selected_session(app: &mut AppState) -> Vec<Effect> {
    let Some(session) = app.sessions.get(app.session_index) else {
        return Vec::new();
    };
    if session.id == app.session_id {
        app.focus = state::Focus::Chat;
        return Vec::new();
    }
    if app.responding {
        app.error = Some(
            "Wait for the current response or cancel it first / 请先等待或取消当前回复".into(),
        );
        return Vec::new();
    }
    vec![Effect::Backend(BackendCommand::Resume(session.id.clone()))]
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
            0 => app.reduce(AppEvent::NewSession),
            1 => vec![Effect::Sessions],
            2 => app.reduce(AppEvent::NewSession),
            3 => vec![Effect::ConfigureModel],
            4 => vec![Effect::Login],
            5 => {
                let language = match app.language {
                    Language::ZhCn => Language::En,
                    Language::En => Language::ZhCn,
                };
                app.reduce(AppEvent::SwitchLanguage(language))
            }
            6 => app.reduce(AppEvent::OpenSearch),
            7 => {
                trust.allow_once();
                app.overlay = None;
                Vec::new()
            }
            8 => {
                app.overlay = None;
                app.screen = state::Screen::Main;
                app.messages.push(Message {
                    role: MessageRole::System,
                    content: format!("{} · {}", app.model, app.workspace.display()),
                    expanded: true,
                });
                Vec::new()
            }
            _ => app.reduce(AppEvent::Exit(TuiExit::Quit)),
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
        app.focus = state::Focus::Chat;
        &mut app.composer
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BackendCommand, Effect, handle_slash_completion_key, resume_selected_session,
        submit_or_command,
    };
    use crate::tui::{
        i18n::Language,
        state::{AppState, Screen},
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use lato_workspace::SessionTrust;
    use std::path::PathBuf;

    #[test]
    fn selected_historical_session_requests_resume() {
        let mut app = AppState::new(
            Language::En,
            PathBuf::from("/workspace"),
            "provider/model".into(),
            "current".into(),
            vec!["current".into(), "historical".into()],
        );
        app.session_index = 1;

        let effects = resume_selected_session(&mut app);
        assert!(!app.should_exit);
        assert!(
            matches!(&effects[..], [Effect::Backend(BackendCommand::Resume(id))] if id == "historical")
        );
    }

    #[test]
    fn session_slash_commands_preserve_title_and_confirm_delete() {
        let workspace = tempfile::tempdir().unwrap();
        let trust = SessionTrust::for_interactive(workspace.path(), false);
        let mut app = AppState::new(
            Language::En,
            workspace.path().to_path_buf(),
            "provider/model".into(),
            "current".into(),
            vec!["current".into()],
        );
        app.screen = Screen::Main;

        app.composer.insert_str("/rename Keep This Case");
        assert!(matches!(
            &submit_or_command(&mut app, &trust)[..],
            [Effect::Backend(BackendCommand::RenameSession { session_id, title })]
                if session_id == "current" && title == "Keep This Case"
        ));

        app.composer.insert_str("/delete");
        assert!(matches!(
            &submit_or_command(&mut app, &trust)[..],
            [Effect::ConfirmDeleteSession(session_id)] if session_id == "current"
        ));
    }

    #[test]
    fn new_and_clear_request_a_fresh_session_without_premature_reset() {
        let workspace = tempfile::tempdir().unwrap();
        let trust = SessionTrust::for_interactive(workspace.path(), false);
        for command in ["/new", "/clear"] {
            let mut app = AppState::new(
                Language::En,
                workspace.path().to_path_buf(),
                "provider/model".into(),
                "current".into(),
                vec!["current".into()],
            );
            app.screen = Screen::Main;
            app.messages.push(crate::tui::state::Message {
                role: crate::tui::state::MessageRole::User,
                content: "keep until acknowledged".into(),
                expanded: true,
            });
            let original_messages = app.messages.clone();
            app.composer.insert_str(command);

            let effects = submit_or_command(&mut app, &trust);

            assert!(matches!(
                effects.as_slice(),
                [Effect::Backend(BackendCommand::NewSession)]
            ));
            assert_eq!(app.session_id, "current");
            assert_eq!(app.screen, Screen::Main);
            assert_eq!(app.messages, original_messages);
        }
    }

    #[test]
    fn slash_completion_filters_navigates_completes_and_executes() {
        let workspace = tempfile::tempdir().unwrap();
        let trust = SessionTrust::for_interactive(workspace.path(), false);
        let mut app = AppState::new(
            Language::En,
            workspace.path().to_path_buf(),
            "provider/model".into(),
            "current".into(),
            vec![],
        );

        app.composer.insert_str("/");
        app.refresh_slash_completion();
        assert_eq!(app.slash_completion().len(), 17);
        assert!(
            handle_slash_completion_key(
                &mut app,
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                &trust,
            )
            .is_some()
        );
        assert_eq!(app.slash_completion_index, 1);
        let effects = handle_slash_completion_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &trust,
        )
        .unwrap();
        assert!(effects.is_empty());
        assert_eq!(app.composer.as_str(), "/new");

        app.composer.replace("/MO");
        app.refresh_slash_completion();
        assert_eq!(app.slash_completion()[0].name, "/model");
        assert!(
            handle_slash_completion_key(
                &mut app,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &trust,
            )
            .unwrap()
            .is_empty()
        );
        assert_eq!(app.composer.as_str(), "/model");
        assert!(matches!(
            &handle_slash_completion_key(
                &mut app,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &trust,
            )
            .unwrap()[..],
            [Effect::ConfigureModel]
        ));
    }

    #[test]
    fn slash_completion_escape_and_edits_update_visibility() {
        let workspace = tempfile::tempdir().unwrap();
        let trust = SessionTrust::for_interactive(workspace.path(), false);
        let mut app = AppState::new(
            Language::En,
            workspace.path().to_path_buf(),
            "provider/model".into(),
            "current".into(),
            vec![],
        );
        app.composer.insert_str("/mo");
        app.refresh_slash_completion();
        assert_eq!(app.slash_completion().len(), 1);
        handle_slash_completion_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &trust,
        );
        assert_eq!(app.composer.as_str(), "/mo");
        assert!(app.slash_completion().is_empty());

        app.composer.backspace();
        app.refresh_slash_completion();
        assert!(!app.slash_completion().is_empty());
    }

    #[tokio::test]
    async fn slash_commands_and_palette_stay_in_the_tui() {
        use super::*;
        let workspace = tempfile::tempdir().unwrap();
        let trust = SessionTrust::for_interactive(workspace.path(), false);
        let mut app = AppState::new(
            Language::En,
            workspace.path().to_path_buf(),
            "provider/model".into(),
            "current".into(),
            vec![],
        );
        app.screen = state::Screen::Main;
        let client = crate::client::InteractiveAcpClient::new_session_with_approval(
            workspace.path().to_path_buf(),
            workspace.path().join("home"),
            trust.clone(),
            lato_agent::default_fake_stream(),
            None,
        )
        .await
        .unwrap();
        tokio::task::LocalSet::new()
            .run_until(async {
                let (backend, _) = backend::spawn(client);
                handle_terminal_event(&mut app, Event::Paste("/sta".into()), &backend, &trust);
                assert_eq!(app.slash_completion().len(), 1);
                assert_eq!(app.slash_completion()[0].name, "/status");
                app.composer.clear();
                app.refresh_slash_completion();
                handle_key(
                    &mut app,
                    KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
                    &backend,
                    &trust,
                );
                assert_eq!(app.composer.as_str(), "/");
                assert!(app.overlay.is_none());
                app.composer.insert_str("model");
                assert!(matches!(
                    &submit_or_command(&mut app, &trust)[..],
                    [Effect::ConfigureModel]
                ));
                assert!(!app.should_exit);
                app.palette_index = 4;
                assert!(matches!(
                    &handle_palette_key(
                        &mut app,
                        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                        &trust
                    )[..],
                    [Effect::Login]
                ));
                assert!(!app.should_exit);
                app.overlay = None;
                app.focus = state::Focus::Tools;
                app.composer.insert_str("pending draft");
                app.reduce(AppEvent::Backend(backend::BackendEvent::Update(
                    crate::client::ClientUpdate::ToolStarted {
                        id: "test-tool".into(),
                        name: "read".into(),
                        arguments: "{}".into(),
                    },
                )));
                let draft = app.composer.as_str().to_string();
                for code in [
                    KeyCode::Enter,
                    KeyCode::Char(' '),
                    KeyCode::Right,
                    KeyCode::Left,
                    KeyCode::Home,
                    KeyCode::End,
                    KeyCode::PageDown,
                ] {
                    let effects = handle_key(
                        &mut app,
                        KeyEvent::new(code, KeyModifiers::NONE),
                        &backend,
                        &trust,
                    );
                    assert!(effects.is_empty());
                    assert_eq!(app.composer.as_str(), draft);
                    assert_eq!(app.focus, state::Focus::Tools);
                    assert!(!app.responding);
                }
            })
            .await;
    }
}
