use super::{
    state::{AppState, Focus, LayoutMode, Overlay, Screen},
    widgets,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    widgets::Block,
};

pub fn render(frame: &mut Frame<'_>, app: &mut AppState) {
    frame.render_widget(
        Block::default().style(ratatui::style::Style::default().bg(widgets::BG)),
        frame.area(),
    );
    if app.layout == LayoutMode::TooSmall {
        widgets::too_small(frame, app);
        return;
    }
    match app.screen {
        Screen::Welcome => {
            let mut welcome_area = frame.area();
            welcome_area.height = welcome_area.height.saturating_sub(3);
            widgets::welcome(frame, welcome_area, app);
            let footer = frame.area();
            let footer = ratatui::layout::Rect::new(
                footer.x,
                footer.bottom().saturating_sub(3),
                footer.width,
                3,
            );
            widgets::footer(frame, footer, app);
        }
        Screen::Main => main(frame, app),
    }
    match app.overlay {
        Some(Overlay::CommandPalette) => widgets::command_palette(frame, app),
        Some(Overlay::Search) => widgets::search_overlay(frame, app),
        Some(Overlay::Configuration) | None => {}
    }
    widgets::approval(frame, app);
}

fn main(frame: &mut Frame<'_>, app: &mut AppState) {
    let rows = Layout::vertical([Constraint::Min(5), Constraint::Length(3)]).split(frame.area());
    match (app.layout, app.focus) {
        (LayoutMode::TooSmall, _) => unreachable!("handled by render"),
        (_, Focus::Chat) => widgets::chat(frame, rows[0], app),
        (LayoutMode::Narrow, Focus::Sessions) => widgets::sessions(frame, rows[0], app),
        (LayoutMode::Narrow, Focus::Tools) => widgets::tools(frame, rows[0], app),
        (_, Focus::Sessions) => {
            let columns =
                Layout::horizontal([Constraint::Length(28), Constraint::Min(40)]).split(rows[0]);
            widgets::sessions(frame, columns[0], app);
            widgets::chat(frame, columns[1], app);
        }
        (_, Focus::Tools) => {
            let columns =
                Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                    .split(rows[0]);
            widgets::chat(frame, columns[0], app);
            widgets::tools(frame, columns[1], app);
        }
    }
    widgets::footer(frame, rows[1], app);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{i18n::Language, state::AppState};
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::PathBuf;

    fn render_text(language: Language, width: u16, height: u16, main: bool) -> String {
        render_text_with_input(language, width, height, main, "", 0)
    }

    fn render_text_with_input(
        language: Language,
        width: u16,
        height: u16,
        main: bool,
        input: &str,
        selected: usize,
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new(
            language,
            PathBuf::from("/tmp/lato"),
            "openai/gpt-test".into(),
            "session-1".into(),
            vec!["session-1".into()],
        );
        app.layout = LayoutMode::for_size(width, height);
        if main {
            app.screen = Screen::Main;
        }
        app.composer = crate::tui::input::InputBuffer::from(input);
        app.slash_completion_index = selected;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut output = String::new();
        for y in 0..height {
            for x in 0..width {
                output.push_str(buffer[(x, y)].symbol());
            }
            output.push('\n');
        }
        output
    }

    #[test]
    fn slash_completion_renders_on_welcome_and_main_screens() {
        for main in [false, true] {
            let all = render_text_with_input(Language::En, 100, 30, main, "/", 0);
            assert!(all.contains("/help"), "{all}");
            assert!(all.contains("/new"), "{all}");

            let filtered = render_text_with_input(Language::En, 100, 30, main, "/mo", 0);
            assert!(filtered.contains("/model"), "{filtered}");
            assert!(!filtered.contains("/help"), "{filtered}");
        }
    }

    #[test]
    fn narrow_slash_completion_keeps_selected_command_and_composer_visible() {
        let text = render_text_with_input(Language::En, 60, 24, true, "/", 18);
        assert!(text.contains("/files"), "{text}");
        assert!(text.contains("› /"), "{text}");
    }

    #[test]
    fn english_welcome_contains_runtime_metadata() {
        let text = render_text(Language::En, 100, 30, false);
        assert!(text.contains("Working directory"));
        assert!(text.contains("Model"));
        assert!(text.contains("Enter a prompt"));
    }

    #[test]
    fn footer_renders_known_context_usage() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new(
            Language::En,
            PathBuf::from("/tmp/lato"),
            "openai/gpt-test".into(),
            "session-1".into(),
            vec!["session-1".into()],
        );
        app.screen = Screen::Main;
        app.context_usage = Some(crate::tui::state::ContextUiState {
            estimated_input_tokens: 850,
            context_window: Some(1_000),
            utilization_percent: Some(85),
        });
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut output = String::new();
        for y in 0..30 {
            for x in 0..100 {
                output.push_str(buffer[(x, y)].symbol());
            }
            output.push('\n');
        }
        assert!(output.contains("context: 850 / 1000 (85%)"), "{output}");
    }

    #[test]
    fn chinese_wide_layout_prioritizes_chat() {
        let text = render_text(Language::ZhCn, 120, 30, true);
        assert!(!text.contains("工 具 调 用"));
        assert!(text.contains("/skills"));
        assert!(text.contains("输 入 消 息"));
    }

    #[test]
    fn narrow_layout_keeps_chat_usable() {
        let text = render_text(Language::En, 60, 24, true);
        assert!(text.contains("Type a message"));
        assert!(!text.contains("Tool calls"));
    }
    #[test]
    fn cursor_tracks_rendered_unicode_input_across_layouts() {
        for (width, main) in [(120, true), (90, true), (60, true), (100, false)] {
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            let mut app = AppState::new(
                Language::En,
                PathBuf::from("/tmp"),
                "model".into(),
                "session".into(),
                vec![],
            );
            app.layout = LayoutMode::for_size(width, 30);
            app.screen = if main { Screen::Main } else { Screen::Welcome };
            for text in ["ab", "中文", "e\u{301}🙂", &"中文🙂abc".repeat(40)] {
                app.composer = crate::tui::input::InputBuffer::from(text);
                terminal.draw(|frame| render(frame, &mut app)).unwrap();
                let cursor = terminal.get_cursor_position().unwrap();
                let buffer = terminal.backend().buffer();
                let marker = (0..width)
                    .find(|x| buffer[(*x, cursor.y)].symbol() == "›")
                    .expect("cursor must be on the input text row, not the border");
                assert!(cursor.x >= marker + 2 && cursor.x < width);
                assert_eq!(
                    buffer[(cursor.x, cursor.y)].symbol(),
                    " ",
                    "cursor must follow the visible text"
                );
                if text == "中文" {
                    assert_eq!(cursor.x, marker + 6);
                }
                if text == "e\u{301}🙂" {
                    assert_eq!(cursor.x, marker + 5);
                }
            }
            app.composer = crate::tui::input::InputBuffer::from("中文ab");
            app.composer.move_left();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let cursor = terminal.get_cursor_position().unwrap();
            assert_eq!(
                terminal.backend().buffer()[(cursor.x, cursor.y)].symbol(),
                "b"
            );
        }
    }

    #[test]
    fn search_cursor_uses_content_row_and_scrolls() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut app = AppState::new(
            Language::En,
            PathBuf::from("/tmp"),
            "model".into(),
            "session".into(),
            vec![],
        );
        app.overlay = Some(Overlay::Search);
        app.search = crate::tui::input::InputBuffer::from("中文abc".repeat(30));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        let buffer = terminal.backend().buffer();
        assert!((0..80).any(|x| buffer[(x, cursor.y)].symbol() == "/"));
        assert_eq!(buffer[(cursor.x, cursor.y)].symbol(), " ");
    }
}
