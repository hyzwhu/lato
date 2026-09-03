use super::{
    state::{AppState, Focus, LayoutMode, Overlay, Screen},
    widgets,
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
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
            widgets::welcome(frame, frame.area(), app);
            let footer = frame.area();
            let footer = ratatui::layout::Rect::new(
                footer.x,
                footer.bottom().saturating_sub(1),
                footer.width,
                1,
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
    let rows = Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).split(frame.area());
    match app.layout {
        LayoutMode::Wide => {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(20),
                    Constraint::Percentage(55),
                    Constraint::Percentage(25),
                ])
                .split(rows[0]);
            widgets::sessions(frame, columns[0], app);
            widgets::chat(frame, columns[1], app);
            widgets::tools(frame, columns[2], app);
        }
        LayoutMode::Medium => {
            let columns =
                Layout::horizontal([Constraint::Percentage(28), Constraint::Percentage(72)])
                    .split(rows[0]);
            if app.focus == Focus::Tools {
                widgets::chat(frame, columns[0], app);
                widgets::tools(frame, columns[1], app);
            } else {
                widgets::sessions(frame, columns[0], app);
                widgets::chat(frame, columns[1], app);
            }
        }
        LayoutMode::Narrow => match app.focus {
            Focus::Sessions => widgets::sessions(frame, rows[0], app),
            Focus::Chat => widgets::chat(frame, rows[0], app),
            Focus::Tools => widgets::tools(frame, rows[0], app),
        },
        LayoutMode::TooSmall => unreachable!("handled by render"),
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
    fn english_welcome_contains_runtime_metadata() {
        let text = render_text(Language::En, 100, 30, false);
        assert!(text.contains("Working directory"));
        assert!(text.contains("Model"));
        assert!(text.contains("Enter a prompt"));
    }

    #[test]
    fn chinese_wide_layout_renders_three_panels() {
        let text = render_text(Language::ZhCn, 120, 30, true);
        assert!(text.contains("会 话"), "{text}");
        assert!(text.contains("工 具 调 用"));
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
