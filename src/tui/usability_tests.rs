use super::*;
use ratatui::{Terminal, backend::TestBackend};

fn app(root: &std::path::Path) -> AppState {
    AppState::new(
        Language::En,
        root.into(),
        "fixture/model".into(),
        "s".into(),
        vec![],
    )
}

#[test]
fn file_selection_replaces_cursor_token_without_sending_or_losing_suffix() {
    let root = tempfile::tempdir().unwrap();
    let trust = SessionTrust::for_interactive(root.path(), false);
    let mut app = app(root.path());
    app.files = vec!["src/中文 file.rs".into()];
    app.composer.replace("inspect @src/ trailing");
    for _ in 0..9 {
        app.composer.move_left();
    }
    let effects = handle_slash_completion_key(
        &mut app,
        KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        &trust,
    )
    .unwrap();
    assert!(effects.is_empty());
    assert_eq!(
        app.composer.as_str(),
        "inspect @\"src/中文 file.rs\"  trailing"
    );
    assert!(!app.responding);
    assert!(app.candidates().is_empty());
}

#[test]
fn escaping_candidates_preserves_draft_and_unknown_commands_are_not_lost() {
    let root = tempfile::tempdir().unwrap();
    let trust = SessionTrust::for_interactive(root.path(), false);
    let mut app = app(root.path());
    app.composer.replace("@");
    assert!(
        handle_slash_completion_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &trust
        )
        .is_some()
    );
    assert_eq!(app.composer.as_str(), "@");
    assert!(app.completion_hint().is_none());
    app.composer.replace("/not-a-command");
    assert!(submit_or_command(&mut app, &trust).is_empty());
    assert_eq!(app.composer.as_str(), "/not-a-command");
    assert!(app.error.is_some());
}

#[test]
fn file_reads_are_prepared_before_composer_is_cleared() {
    let root = tempfile::tempdir().unwrap();
    let mut app = app(root.path());
    app.composer.replace("inspect @missing.txt");
    let effects = app.reduce(AppEvent::Submit);
    assert!(matches!(&effects[..], [Effect::PrepareSubmit(_)]));
    assert_eq!(app.composer.as_str(), "inspect @missing.txt");
    assert!(context::expand_references(root.path(), app.composer.as_str()).is_err());
    assert!(!app.responding);
    assert!(app.messages.is_empty());
}

#[test]
fn workflows_command_requests_a_backend_list() {
    let root = tempfile::tempdir().unwrap();
    let trust = SessionTrust::for_interactive(root.path(), false);
    let mut app = app(root.path());
    app.composer.replace("/workflows");
    let effects = submit_or_command(&mut app, &trust);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Backend(BackendCommand::ListWorkflows)]
        ),
        "{effects:?}"
    );
    app.reduce(AppEvent::Backend(backend::BackendEvent::Workflows(
        crate::client::WorkflowListResponse {
            generation: 3,
            workflows: vec![crate::client::WorkflowEntry {
                id: "demo/review".into(),
                name: "review".into(),
                description: "Review the diff".into(),
                steps: 1,
                agent_budget: 128,
            }],
        },
    )));
    assert_eq!(app.workflows[0].id, "demo/review");
    assert!(
        app.messages
            .iter()
            .any(|message| message.content.contains("demo/review"))
    );
}

#[test]
fn skill_candidates_invoke_user_backend_with_arguments() {
    let root = tempfile::tempdir().unwrap();
    let trust = SessionTrust::for_interactive(root.path(), false);
    let mut app = app(root.path());
    app.skills = vec![crate::client::SkillEntry {
        qualified_name: "demo:inspect".into(),
        name: "inspect".into(),
        description: "Inspect source".into(),
        argument_hint: Some("<file>".into()),
        source: "demo".into(),
    }];
    app.composer.replace("/demo");
    handle_slash_completion_key(
        &mut app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &trust,
    )
    .unwrap();
    assert_eq!(app.composer.as_str(), "/demo:inspect ");
    assert!(!app.responding);
    app.composer.insert_str("中文.rs");
    let effects = submit_or_command(&mut app, &trust);
    assert!(
        matches!(&effects[..],[Effect::Backend(BackendCommand::InvokeSkill {name,args,..})] if name=="demo:inspect" && args.as_deref()==Some("中文.rs"))
    );
}

#[test]
fn history_restores_unsent_multiline_draft() {
    let root = tempfile::tempdir().unwrap();
    let mut app = app(root.path());
    app.history = vec!["first".into(), "second".into()];
    app.composer.replace("未发送\n草稿");
    app.recall_history(-1);
    assert_eq!(app.composer.as_str(), "second");
    app.recall_history(-1);
    assert_eq!(app.composer.as_str(), "first");
    app.recall_history(1);
    app.recall_history(1);
    assert_eq!(app.composer.as_str(), "未发送\n草稿");
}

#[test]
fn fuzzy_palette_uses_same_registry_and_hides_alias_duplicates() {
    let root = tempfile::tempdir().unwrap();
    let app = app(root.path());
    let candidates = app.command_candidates("mdl", 0..0);
    assert_eq!(candidates[0].name, "/model");
    let candidates = app.command_candidates("", 0..0);
    assert!(!candidates.iter().any(|c| c.name == "/quit"));
    assert!(
        app.command_candidates("quit", 0..0)
            .iter()
            .any(|c| c.name == "/quit")
    );
}

#[test]
fn multiline_render_keeps_cursor_and_completions_visible_at_small_sizes() {
    for (width, height) in [(46, 14), (60, 24), (90, 30), (120, 30)] {
        for screen in [state::Screen::Welcome, state::Screen::Main] {
            let mut app = app(std::path::Path::new("/tmp"));
            app.screen = screen;
            app.layout = state::LayoutMode::for_size(width, height);
            app.composer
                .replace(&format!("{}\n/", "中文🙂 abc\n".repeat(12)));
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render::render(frame, &mut app))
                .unwrap();
            let cursor = terminal.get_cursor_position().unwrap();
            assert!(cursor.x < width && cursor.y < height);
            assert_eq!(
                terminal.backend().buffer()[(cursor.x - 1, cursor.y)].symbol(),
                "/"
            );
        }
    }
}

#[test]
fn palette_parks_draft_without_invalid_history_index() {
    let root = tempfile::tempdir().unwrap();
    let trust = SessionTrust::for_interactive(root.path(), false);
    let mut app = app(root.path());
    app.composer.replace("unsent draft");
    app.palette_query.replace("status");
    super::handle_palette_key(
        &mut app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &trust,
    );
    assert_eq!(app.parked_draft.as_deref(), Some("unsent draft"));
    app.recall_history(-1); // Empty history must not panic.
    app.composer.clear();
    app.restore_parked_draft();
    assert_eq!(app.composer.as_str(), "unsent draft");
}

#[test]
fn stale_selection_clamps_before_accepting_and_skill_arguments_have_no_false_error() {
    let root = tempfile::tempdir().unwrap();
    let trust = SessionTrust::for_interactive(root.path(), false);
    let mut app = app(root.path());
    app.files = vec!["README.md".into()];
    app.composer.replace("@READ");
    app.slash_completion_index = 99;
    assert!(
        super::handle_slash_completion_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &trust
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(app.composer.as_str(), "@README.md ");
    assert!(!app.responding);
    app.composer.replace("/skill demo:inspect args");
    app.refresh_slash_completion();
    assert!(app.completion_hint().is_none());
}

#[test]
fn chat_follows_latest_message_and_page_up_reveals_older_content() {
    let mut app = app(std::path::Path::new("/tmp"));
    app.screen = state::Screen::Main;
    for i in 0..50 {
        app.messages.push(Message {
            role: MessageRole::Assistant,
            content: format!("message {i}"),
            expanded: true,
        });
    }
    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    terminal.draw(|f| render::render(f, &mut app)).unwrap();
    let visible = |terminal: &Terminal<TestBackend>| {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };
    assert!(visible(&terminal).contains("message 49"));
    app.reduce(AppEvent::Scroll(-10));
    terminal.draw(|f| render::render(f, &mut app)).unwrap();
    assert!(!visible(&terminal).contains("message 49"));
    app.reduce(AppEvent::Scroll(10));
    terminal.draw(|f| render::render(f, &mut app)).unwrap();
    assert!(visible(&terminal).contains("message 49"));
}

fn update(app: &mut AppState, event: crate::client::ClientUpdate) {
    app.reduce(AppEvent::Backend(backend::BackendEvent::Update(event)));
}

#[test]
fn reasoning_stream_follows_actual_event_order_and_folds_when_answer_starts() {
    use crate::client::ClientUpdate::*;
    let mut app = app(std::path::Path::new("/tmp"));
    app.composer.replace("question");
    app.reduce(AppEvent::Submit);
    update(&mut app, ReasoningDelta("first ".into()));
    update(&mut app, ReasoningDelta("segment".into()));
    assert_eq!(app.messages.len(), 2);
    assert_eq!(app.messages[1].content, "first segment");
    assert!(app.messages[1].expanded);
    update(
        &mut app,
        ToolStarted {
            id: "t1".into(),
            name: "read".into(),
            arguments: "{}".into(),
        },
    );
    assert!(!app.messages[1].expanded);
    assert!(app.active_reasoning.is_none());
    assert!(progress::activity(&app).contains("read"));
    update(
        &mut app,
        ToolFinished {
            id: "t1".into(),
            result: "text".into(),
        },
    );
    update(&mut app, ReasoningDelta("second segment".into()));
    update(&mut app, TextDelta("answer ".into()));
    update(&mut app, TextDelta("continues".into()));
    assert_eq!(
        app.messages.iter().map(|m| m.role).collect::<Vec<_>>(),
        vec![
            MessageRole::User,
            MessageRole::Reasoning,
            MessageRole::Reasoning,
            MessageRole::Assistant
        ]
    );
    assert_eq!(app.messages[3].content, "answer continues");
    assert!(
        app.reasoning_segments
            .iter()
            .all(|segment| segment.duration.is_some())
    );
    app.toggle_reasoning();
    assert!(app.messages[1].expanded && app.messages[2].expanded);
    app.toggle_reasoning();
    assert!(!app.messages[1].expanded && !app.messages[2].expanded);
    app.reduce(AppEvent::Backend(backend::BackendEvent::TurnCompleted(
        "answer continues".into(),
    )));
    assert_eq!(app.generation_phase, state::GenerationPhase::Completed);
    assert!(!app.responding);
}

#[test]
fn no_reasoning_is_invented_and_cancelled_thinking_stops_animating() {
    let mut app = app(std::path::Path::new("/tmp"));
    app.composer.replace("question");
    app.reduce(AppEvent::Submit);
    assert_eq!(progress::activity(&app), "Waiting for model");
    update(
        &mut app,
        crate::client::ClientUpdate::TextDelta("answer".into()),
    );
    assert!(
        !app.messages
            .iter()
            .any(|message| message.role == MessageRole::Reasoning)
    );
    app.reduce(AppEvent::Backend(backend::BackendEvent::TurnCompleted(
        "answer".into(),
    )));
    app.composer.replace("next");
    app.reduce(AppEvent::Submit);
    update(
        &mut app,
        crate::client::ClientUpdate::ReasoningDelta("streamed reasoning".into()),
    );
    let before = progress::spinner(&app);
    app.reduce(AppEvent::Tick);
    assert_ne!(before, progress::spinner(&app));
    app.reduce(AppEvent::Backend(backend::BackendEvent::TurnCancelled));
    assert_eq!(progress::activity(&app), "Cancelled");
    assert!(app.active_reasoning.is_none());
    assert!(app.reasoning_segments.last().unwrap().duration.is_some());
}

#[test]
fn active_reasoning_tail_and_context_remain_visible_at_all_terminal_widths() {
    for width in [46, 60, 90, 120] {
        let mut app = app(std::path::Path::new("/tmp"));
        app.composer.replace("question");
        app.reduce(AppEvent::Submit);
        app.layout = state::LayoutMode::for_size(width, 24);
        update(
            &mut app,
            crate::client::ClientUpdate::ContextUsage {
                estimated_input_tokens: 850,
                context_window: Some(1000),
                utilization_percent: Some(85),
            },
        );
        update(
            &mut app,
            crate::client::ClientUpdate::ReasoningDelta(format!(
                "{}\nlatest-reasoning-marker",
                "earlier reasoning\n".repeat(12)
            )),
        );
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal
            .draw(|frame| render::render(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("latest-reasoning-marker"), "{text}");
        assert!(text.contains("context: 850 / 1000 (85%)"), "{text}");
        let bottom = (21..24)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .map(|pos| terminal.backend().buffer()[pos].symbol())
            .collect::<String>();
        assert!(bottom.contains("context:"));
        assert!(bottom.contains("Thinking"));
        assert!(bottom.contains("model"));
        assert!(bottom.contains("F2"));
    }
}

#[test]
fn context_unknown_and_compression_do_not_misrepresent_usage() {
    let mut app = app(std::path::Path::new("/tmp"));
    app.screen = state::Screen::Main;
    let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
    terminal
        .draw(|frame| render::render(frame, &mut app))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("context: — / —"));
    assert!(!text.contains("(0%)"));
    update(
        &mut app,
        crate::client::ClientUpdate::ContextUsage {
            estimated_input_tokens: 700,
            context_window: None,
            utilization_percent: None,
        },
    );
    update(
        &mut app,
        crate::client::ClientUpdate::CompactionStarted {
            trigger: "manual".into(),
            compaction_id: "test-compact".into(),
        },
    );
    terminal
        .draw(|frame| render::render(frame, &mut app))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("context: 700 / —"));
    assert!(text.contains("Compacting context"));
}

#[test]
fn expanding_reasoning_reveals_it_above_a_long_answer() {
    let mut app = app(std::path::Path::new("/tmp"));
    app.composer.replace("question");
    app.reduce(AppEvent::Submit);
    update(
        &mut app,
        crate::client::ClientUpdate::ReasoningDelta("reveal-reasoning-marker".into()),
    );
    update(
        &mut app,
        crate::client::ClientUpdate::TextDelta("answer line\n".repeat(100)),
    );
    app.reduce(AppEvent::Backend(backend::BackendEvent::TurnCompleted(
        String::new(),
    )));
    let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
    terminal
        .draw(|frame| render::render(frame, &mut app))
        .unwrap();
    app.toggle_reasoning();
    terminal
        .draw(|frame| render::render(frame, &mut app))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("reveal-reasoning-marker"));
    assert!(app.scroll > 0);
}
