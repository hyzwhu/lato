use super::{
    i18n::{TextKey, tr},
    state::{AppState, ToolStatus},
    widgets::{AMBER, BG, BLUE, ERROR, GLASS, MUTED, TEXT, panel_style},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Default)]
pub struct ToolPanelState {
    pub selected: usize,
    pub scroll: usize,
    width: u16,
    height: usize,
    ranges: Vec<Range<usize>>,
    reveal_selection: bool,
}

impl ToolPanelState {
    pub fn select(&mut self, delta: i16, count: usize) {
        self.selected = self
            .selected
            .saturating_add_signed(delta as isize)
            .min(count.saturating_sub(1));
        self.reveal_selection = true;
    }

    pub fn select_last(&mut self, count: usize) {
        self.selected = count.saturating_sub(1);
        self.reveal_selection = true;
    }

    fn page(&mut self, down: bool) {
        let step = self.height.saturating_sub(1).max(1);
        let max = self
            .ranges
            .last()
            .map_or(0, |range| range.end.saturating_sub(self.height));
        self.scroll = if down {
            self.scroll.saturating_add(step).min(max)
        } else {
            self.scroll.saturating_sub(step)
        };
        // Keep selection on a visible card, including when paging inside a long result.
        if let Some(range) = self.ranges.get(self.selected)
            && (range.end <= self.scroll || range.start >= self.scroll + self.height)
        {
            self.selected = self
                .ranges
                .iter()
                .position(|range| range.end > self.scroll)
                .unwrap_or(0);
        }
        self.reveal_selection = false;
    }
}

/// Consume tool navigation before generic composer handling can submit or edit input.
pub fn handle_key(app: &mut AppState, key: KeyEvent) -> bool {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        return false;
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => app.tool_panel.select(-1, app.tools.len()),
        KeyCode::Down | KeyCode::Char('j') => app.tool_panel.select(1, app.tools.len()),
        KeyCode::Home => {
            app.tool_panel.selected = 0;
            app.tool_panel.reveal_selection = true;
        }
        KeyCode::End => app.tool_panel.select_last(app.tools.len()),
        KeyCode::PageUp => app.tool_panel.page(false),
        KeyCode::PageDown => app.tool_panel.page(true),
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right => {
            if let Some(tool) = app.tools.get_mut(app.tool_panel.selected) {
                tool.expanded = match key.code {
                    KeyCode::Left => false,
                    KeyCode::Right => true,
                    _ => !tool.expanded,
                };
                app.tool_panel.reveal_selection = true;
            }
        }
        _ => return false,
    }
    true
}

// Wrap before rendering so the viewport uses actual terminal rows, including Unicode
// graphemes and explicit newlines. This also avoids Paragraph's u16 scroll limit.
fn wrapped_lines(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for source in text.split('\n') {
        let mut line = String::new();
        let mut columns = 0;
        for grapheme in source.graphemes(true) {
            let normalized = if grapheme == "\t" { "    " } else { grapheme };
            if normalized.chars().any(char::is_control) {
                continue;
            }
            let size = normalized.width();
            if columns + size > width && !line.is_empty() {
                lines.push(Line::styled(std::mem::take(&mut line), style));
                columns = 0;
            }
            line.push_str(normalized);
            columns += size;
        }
        lines.push(Line::styled(line, style));
    }
    lines
}

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &mut AppState) {
    let active = app.focus == super::state::Focus::Tools;
    let style = panel_style(active);
    let position = if app.tools.is_empty() {
        String::new()
    } else {
        format!(" {}/{}", app.tool_panel.selected + 1, app.tools.len())
    };
    let block = Block::default()
        .title(format!(
            " {}{position} ",
            tr(app.language, TextKey::ToolCalls)
        ))
        .title_style(
            Style::default()
                .fg(if active { TEXT } else { MUTED })
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::TOP)
        .border_style(Style::default().fg(GLASS))
        .style(style);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.tools.is_empty() {
        frame.render_widget(
            Paragraph::new(tr(app.language, TextKey::WaitingTools))
                .style(Style::default().fg(MUTED).bg(style.bg.unwrap_or(BG))),
            inner,
        );
        return;
    }
    if inner.width < 2 || inner.height == 0 {
        return;
    }
    let content = Rect {
        width: inner.width - 1,
        ..inner
    };
    let mut lines = Vec::new();
    let mut ranges = Vec::with_capacity(app.tools.len());
    for (index, tool) in app.tools.iter().enumerate() {
        let start = lines.len();
        let selected = index == app.tool_panel.selected;
        let row_style = if selected && active {
            style.bg(GLASS)
        } else {
            style
        };
        let (symbol, key, color) = match tool.status {
            ToolStatus::Running => ("●", TextKey::Running, AMBER),
            ToolStatus::Done => ("✓", TextKey::Done, BLUE),
            ToolStatus::Error => ("✗", TextKey::Failed, ERROR),
        };
        let marker = if tool.expanded { "▼" } else { "▶" };
        lines.push(
            Line::from(vec![
                Span::styled(format!("{marker} {symbol} "), row_style.fg(color)),
                Span::styled(
                    tool.name.clone(),
                    row_style.fg(TEXT).add_modifier(Modifier::BOLD),
                ),
            ])
            .style(row_style),
        );
        lines.extend(wrapped_lines(
            &format!("  {} · {}ms", tr(app.language, key), tool.elapsed_ms),
            content.width as usize,
            row_style.fg(color),
        ));
        if tool.expanded {
            lines.extend(wrapped_lines(
                &tool.arguments,
                content.width as usize,
                row_style.fg(MUTED),
            ));
            if let Some(result) = &tool.result {
                lines.extend(wrapped_lines(
                    &format!("{}: {result}", tr(app.language, TextKey::ReturnValue)),
                    content.width as usize,
                    row_style.fg(if tool.status == ToolStatus::Error {
                        ERROR
                    } else {
                        MUTED
                    }),
                ));
            }
        }
        ranges.push(start..lines.len());
    }

    let total = lines.len();
    let panel = &mut app.tool_panel;
    if panel.width != content.width || panel.height != content.height as usize {
        panel.reveal_selection = true;
    }
    panel.width = content.width;
    panel.height = content.height as usize;
    panel.ranges = ranges;
    panel.selected = panel.selected.min(app.tools.len() - 1);
    panel.scroll = panel.scroll.min(total.saturating_sub(panel.height));
    if panel.reveal_selection {
        let header = panel.ranges[panel.selected].start;
        if header < panel.scroll {
            panel.scroll = header;
        } else if header + 2 > panel.scroll + panel.height {
            panel.scroll = (header + 2).saturating_sub(panel.height);
        }
        panel.reveal_selection = false;
    }
    panel.scroll = panel.scroll.min(total.saturating_sub(panel.height));
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(panel.scroll)
                .take(panel.height)
                .collect::<Vec<_>>(),
        )
        .style(style),
        content,
    );
    if total > panel.height {
        let mut scrollbar = ScrollbarState::new(total.saturating_sub(panel.height) + 1)
            .position(panel.scroll)
            .viewport_content_length(panel.height);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .thumb_style(Style::default().fg(AMBER))
                .track_style(Style::default().fg(GLASS)),
            inner,
            &mut scrollbar,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        client::ClientUpdate,
        tui::{
            backend::BackendEvent,
            i18n::Language,
            state::{AppEvent, Focus},
        },
    };
    use ratatui::{Terminal, backend::TestBackend};

    fn app(count: usize) -> AppState {
        let mut app = AppState::new(
            Language::En,
            "/tmp".into(),
            "model".into(),
            "session".into(),
            vec![],
        );
        app.focus = Focus::Tools;
        for i in 0..count {
            app.reduce(AppEvent::Backend(BackendEvent::Update(
                ClientUpdate::ToolStarted {
                    id: i.to_string(),
                    name: format!("tool-{i:03}"),
                    arguments: "hidden arguments".into(),
                },
            )));
        }
        app
    }

    fn draw(app: &mut AppState, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key(app: &mut AppState, code: KeyCode) {
        assert!(handle_key(app, KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn overflow_navigation_and_resize_keep_selected_calls_visible() {
        let mut app = app(50);
        let first = draw(&mut app, 32, 10);
        assert!(first.contains("tool-000"));
        assert!(!first.contains("hidden arguments"));
        assert!(!first.contains("tool-049"));
        key(&mut app, KeyCode::End);
        assert!(draw(&mut app, 32, 10).contains("tool-049"));
        assert!(app.tool_panel.scroll > 0);
        assert!(draw(&mut app, 18, 6).contains("tool-049"));
        key(&mut app, KeyCode::Home);
        assert!(draw(&mut app, 18, 6).contains("tool-000"));
        assert_eq!(app.tool_panel.scroll, 0);
        app.scroll = 12;
        app.reduce(AppEvent::Scroll(1));
        assert_eq!(app.tool_panel.selected, 1);
        assert_eq!(app.scroll, 12);
    }

    #[test]
    fn complete_multiline_results_are_pageable_and_collapse_clamps_scroll() {
        let mut app = app(1);
        app.tools[0].result = Some(format!(
            "{}\nFINAL_RESULT",
            "中文🙂 e\u{301} payload\n".repeat(100)
        ));
        key(&mut app, KeyCode::Enter);
        let text = draw(&mut app, 24, 8);
        assert!(text.contains("hidden arguments"));
        // TestBackend includes a blank continuation cell after each wide glyph.
        assert!(text.replace(' ', "").contains("中文🙂"));
        assert!(!text.contains("FINAL_RESULT"));
        for _ in 0..100 {
            key(&mut app, KeyCode::PageDown);
            draw(&mut app, 24, 8);
        }
        assert!(draw(&mut app, 24, 8).contains("FINAL_RESULT"));
        assert_eq!(app.tool_panel.selected, 0);
        key(&mut app, KeyCode::Left);
        let collapsed = draw(&mut app, 24, 8);
        assert!(collapsed.contains("tool-000"));
        assert!(!collapsed.contains("FINAL_RESULT"));
        assert_eq!(app.tool_panel.scroll, 0);
        key(&mut app, KeyCode::Right);
        assert!(draw(&mut app, 24, 8).contains("hidden arguments"));
    }

    #[test]
    fn paging_moves_selection_to_visible_calls_and_clamps_at_top() {
        let mut app = app(50);
        draw(&mut app, 32, 10);
        key(&mut app, KeyCode::PageDown);
        assert!(app.tool_panel.selected > 0);
        let selected = format!("tool-{:03}", app.tool_panel.selected);
        assert!(draw(&mut app, 32, 10).contains(&selected));
        key(&mut app, KeyCode::PageUp);
        key(&mut app, KeyCode::PageUp);
        assert_eq!(app.tool_panel.scroll, 0);
    }

    #[test]
    fn wrapping_preserves_newlines_and_graphemes() {
        let lines = wrapped_lines("中文🙂e\u{301}\n\nend", 4, Style::default());
        let text: Vec<_> = lines.iter().map(Line::to_string).collect();
        assert_eq!(text, ["中文", "🙂e\u{301}", "", "end"]);
        assert!(lines.iter().all(|line| line.width() <= 4));
    }

    #[test]
    fn empty_panel_navigation_is_safe() {
        let mut app = app(0);
        for code in [
            KeyCode::Enter,
            KeyCode::Char(' '),
            KeyCode::End,
            KeyCode::Down,
            KeyCode::PageDown,
        ] {
            key(&mut app, code);
        }
        assert!(draw(&mut app, 32, 10).contains("Waiting for tool calls"));
        assert_eq!(app.tool_panel.selected, 0);
    }
}
