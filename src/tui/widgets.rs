use super::{
    i18n::{Language, TextKey, tr},
    state::{AppState, Focus, MessageRole},
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

pub const BG: Color = Color::Rgb(26, 27, 30);
pub const RAISED: Color = Color::Rgb(31, 33, 37);
pub const GLASS: Color = Color::Rgb(37, 40, 45);
pub const AMBER: Color = Color::Rgb(229, 168, 64);
pub const BLUE: Color = Color::Rgb(91, 156, 246);
pub const TEXT: Color = Color::Rgb(201, 209, 217);
pub const MUTED: Color = Color::Rgb(110, 118, 129);
pub const ERROR: Color = Color::Rgb(248, 113, 113);

pub fn welcome(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(2),
            Constraint::Length(6),
            Constraint::Length(1),
            Constraint::Length(5),
            Constraint::Length(composer_height(app, centered_rect(area, 90, 1).width)),
            Constraint::Min(2),
        ])
        .split(area);
    let logo = r#" _          _       
| |    __ _| |_ ___ 
| |   / _` | __/ _ \
| |__| (_| | || (_) |
|_____\__,_|\__\___/"#;
    frame.render_widget(
        Paragraph::new(logo).alignment(Alignment::Center).style(
            Style::default()
                .fg(TEXT)
                .bg(BG)
                .add_modifier(Modifier::BOLD),
        ),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(format!("v{}", env!("CARGO_PKG_VERSION")))
            .alignment(Alignment::Center)
            .style(Style::default().fg(MUTED).bg(BG)),
        rows[2],
    );
    let info = Text::from(vec![
        centered_label(
            tr(app.language, TextKey::WorkingDirectory),
            &app.workspace.display().to_string(),
        ),
        centered_label(tr(app.language, TextKey::Model), &app.model),
        centered_label(
            tr(app.language, TextKey::ApiStatus),
            &format!("● {}", tr(app.language, TextKey::Online)),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(info)
            .alignment(Alignment::Center)
            .style(Style::default().fg(TEXT).bg(BG)),
        rows[3],
    );
    let input_area = centered_rect(
        rows[4],
        90,
        composer_height(app, centered_rect(area, 90, 1).width),
    );
    composer(frame, input_area, app, true);
    if let Some(error) = &app.error {
        let error_area = Rect::new(area.x, area.bottom().saturating_sub(3), area.width, 1);
        frame.render_widget(
            Paragraph::new(format!("{}: {error}", tr(app.language, TextKey::Error)))
                .alignment(Alignment::Center)
                .style(Style::default().fg(ERROR).bg(BG)),
            error_area,
        );
    }
}

fn centered_label(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(MUTED)),
        Span::styled(value.to_string(), Style::default().fg(TEXT)),
    ])
}

pub fn sessions(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let active = app.focus == Focus::Sessions;
    let style = panel_style(active);
    let block = Block::default()
        .title(format!(" {}  [+] ", tr(app.language, TextKey::Sessions)))
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
    let items = app.sessions.iter().enumerate().map(|(index, session)| {
        let marker = if session.id == app.session_id {
            "●"
        } else {
            " "
        };
        ListItem::new(Line::from(vec![
            Span::styled(format!("{marker} "), Style::default().fg(AMBER)),
            Span::styled(session.title.clone(), Style::default().fg(TEXT)),
            Span::styled(
                format!(" · {}", compact_id(&session.id)),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                format!(" {}", session.timestamp),
                Style::default().fg(MUTED),
            ),
        ]))
        .style(if active && index == app.session_index {
            Style::default().bg(Color::Rgb(45, 48, 52))
        } else {
            Style::default()
        })
    });
    frame.render_widget(List::new(items).style(style), inner);
    if app.sessions.is_empty() {
        frame.render_widget(
            Paragraph::new(format!("[{}]", tr(app.language, TextKey::NewSession)))
                .alignment(Alignment::Center)
                .style(Style::default().fg(MUTED).bg(style.bg.unwrap_or(BG))),
            inner,
        );
    }
}

pub fn chat(frame: &mut Frame<'_>, area: Rect, app: &mut AppState) {
    let active = app.focus == Focus::Chat;
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(composer_height(app, area.width)),
    ])
    .split(area);
    let background = panel_style(active);
    frame.render_widget(Block::default().style(background), area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(
                    "{}: {}",
                    tr(app.language, TextKey::Path),
                    app.workspace.display()
                ),
                Style::default().fg(MUTED),
            ),
            Span::raw("    "),
            Span::styled(app.model.clone(), Style::default().fg(MUTED)),
        ]))
        .style(Style::default().bg(GLASS)),
        rows[0],
    );
    let mut lines = Vec::new();
    let mut reveal_line = None;
    for (message_index, message) in app.messages.iter().enumerate() {
        if app.reveal_reasoning == Some(message_index) {
            reveal_line = Some(lines.len());
        }
        match message.role {
            MessageRole::User => {
                lines.push(Line::from(vec![
                    Span::styled(
                        "› ",
                        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(message.content.clone(), Style::default().fg(TEXT)),
                ]));
            }
            MessageRole::Assistant => {
                for line in message.content.lines() {
                    lines.push(Line::styled(format!("  {line}"), Style::default().fg(TEXT)));
                }
                if message.content.is_empty() && app.responding {
                    lines.push(Line::styled("  ···", Style::default().fg(BLUE)));
                }
            }
            MessageRole::Reasoning => {
                let active = app.active_reasoning == Some(message_index);
                let segment = app
                    .reasoning_segments
                    .iter()
                    .find(|segment| segment.message_index == message_index);
                let seconds = segment
                    .map(|segment| {
                        segment
                            .duration
                            .unwrap_or_else(|| segment.started_at.elapsed())
                            .as_secs_f32()
                    })
                    .unwrap_or(0.0);
                let label = match (app.language, active) {
                    (Language::ZhCn, true) => "思考中",
                    (Language::ZhCn, false) => "思考记录",
                    (Language::En, true) => "Thinking",
                    (Language::En, false) => "Thought",
                };
                let marker = if active {
                    super::progress::spinner(app)
                } else if message.expanded {
                    "▾"
                } else {
                    "▸"
                };
                lines.push(Line::styled(
                    format!(
                        "{marker} {label} · {seconds:.1}s · {} {} · F2",
                        message.content.chars().count(),
                        if app.language == Language::ZhCn {
                            "字"
                        } else {
                            "chars"
                        }
                    ),
                    Style::default().fg(AMBER),
                ));
                if message.expanded {
                    let body = wrap_transcript(
                        vec![Line::from(Span::styled(
                            message.content.clone(),
                            Style::default().fg(MUTED),
                        ))],
                        rows[1].width.saturating_sub(4) as usize,
                    );
                    let start = if active {
                        body.len().saturating_sub(6)
                    } else {
                        0
                    };
                    if start > 0 {
                        lines.push(Line::styled(
                            format!(
                                "  │ … {start} {}",
                                if app.language == Language::ZhCn {
                                    "行"
                                } else {
                                    "earlier lines"
                                }
                            ),
                            Style::default().fg(MUTED),
                        ));
                    }
                    for mut line in body.into_iter().skip(start) {
                        line.spans
                            .insert(0, Span::styled("  │ ", Style::default().fg(AMBER)));
                        lines.push(line);
                    }
                    lines.push(Line::styled("  ╰─", Style::default().fg(AMBER)));
                }
            }
            MessageRole::System => {
                for line in message.content.lines() {
                    lines.push(Line::styled(line.to_string(), Style::default().fg(MUTED)));
                }
            }
        }
        lines.push(Line::raw(""));
    }
    if let Some(error) = &app.error {
        lines.push(Line::styled(
            format!("{}: {error}", tr(app.language, TextKey::Error)),
            Style::default().fg(ERROR),
        ));
    }
    let reveal_line = reveal_line
        .map(|index| wrap_transcript(lines[..index].to_vec(), rows[1].width as usize).len());
    let lines = wrap_transcript(lines, rows[1].width as usize);
    let max_scroll = lines
        .len()
        .saturating_sub(rows[1].height as usize)
        .min(u16::MAX as usize) as u16;
    if app.scroll > 0 {
        app.scroll = app
            .scroll
            .saturating_add(max_scroll.saturating_sub(app.scroll_max))
            .min(max_scroll);
    }
    app.scroll_max = max_scroll;
    if let Some(line) = reveal_line {
        app.scroll = max_scroll.saturating_sub(line.min(u16::MAX as usize) as u16);
        app.reveal_reasoning = None;
    }
    let start = lines
        .len()
        .saturating_sub(rows[1].height as usize)
        .saturating_sub(app.scroll as usize);
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(start)
                .take(rows[1].height as usize)
                .collect::<Vec<_>>(),
        )
        .style(background),
        rows[1],
    );
    composer(frame, rows[2], app, false);
}

fn composer(frame: &mut Frame<'_>, area: Rect, app: &AppState, welcome: bool) {
    let rows = Layout::vertical([Constraint::Min(2), Constraint::Length(1)]).split(area);
    let placeholder = if welcome {
        tr(app.language, TextKey::StartPrompt)
    } else {
        tr(app.language, TextKey::MessagePlaceholder)
    };
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(GLASS));
    let inner = block.inner(rows[0]);
    frame.render_widget(block.style(Style::default().bg(BG)), rows[0]);
    let (visible, cursor, cursor_row) = app.composer.multiline_viewport(
        inner.width.saturating_sub(2) as usize,
        inner.height as usize,
    );
    let lines = if app.composer.is_empty() {
        vec![Line::from(vec![
            Span::styled("› ", Style::default().fg(AMBER)),
            Span::styled(placeholder, Style::default().fg(MUTED)),
        ])]
    } else {
        visible
            .into_iter()
            .enumerate()
            .map(|(row, text)| {
                Line::from(vec![
                    Span::styled(
                        if row == cursor_row { "› " } else { "  " },
                        Style::default().fg(AMBER),
                    ),
                    Span::styled(text, Style::default().fg(TEXT)),
                ])
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines).style(Style::default().bg(BG)), inner);
    let mut status = if app.is_busy() {
        format!(
            "{} · Ctrl+C {}",
            super::progress::status_line(app),
            if app.language == Language::ZhCn {
                "取消"
            } else {
                "stop"
            }
        )
    } else {
        match app.language {
            Language::ZhCn => "/ 命令 · @ 文件 · /skills 技能".into(),
            Language::En => "/ commands · @ files · /skills skills".into(),
        }
    };
    let references = super::context::reference_count(app.composer.as_str());
    if references > 0 {
        status = format!(
            "@ {references} {} · {status}",
            match app.language {
                Language::ZhCn => "个文件",
                Language::En => "files",
            }
        );
    }
    frame.render_widget(
        Paragraph::new(status)
            .alignment(if welcome {
                Alignment::Center
            } else {
                Alignment::Left
            })
            .style(
                Style::default()
                    .fg(if app.is_busy() { AMBER } else { MUTED })
                    .bg(BG),
            ),
        rows[1],
    );
    slash_completion(frame, area, app);
    if app.focus == Focus::Chat
        && app.overlay.is_none()
        && app.approval.is_none()
        && inner.width > 2
        && inner.height > 0
    {
        frame.set_cursor_position((inner.x + 2 + cursor as u16, inner.y + cursor_row as u16));
    }
}

#[cfg(test)]
fn compaction_trigger_label(trigger: &str, language: Language) -> &'static str {
    match (language, trigger) {
        (Language::ZhCn, "model_switch") => "模型切换",
        (Language::ZhCn, "threshold") => "自动阈值",
        (Language::ZhCn, "preflight_overflow") => "工具输出溢出",
        (Language::ZhCn, "provider_overflow") => "服务端上下文溢出恢复",
        (Language::ZhCn, _) => "手动",
        (Language::En, "model_switch") => "model switch",
        (Language::En, "threshold") => "automatic threshold",
        (Language::En, "preflight_overflow") => "tool-output overflow",
        (Language::En, "provider_overflow") => "provider overflow recovery",
        (Language::En, _) => "manual",
    }
}

fn composer_height(app: &AppState, width: u16) -> u16 {
    app.composer
        .visual_line_count(width.saturating_sub(2) as usize)
        .clamp(1, 8) as u16
        + 2
}

pub fn slash_completion(frame: &mut Frame<'_>, composer_area: Rect, app: &AppState) {
    if app.overlay.is_some() || app.approval.is_some() {
        return;
    }
    let candidates = app.candidates();
    let hint = app.completion_hint();
    if candidates.is_empty() && hint.is_none() {
        return;
    }
    let popup_height = (candidates.len().max(1) as u16 + 2)
        .min(10)
        .min(composer_area.y.saturating_sub(frame.area().y));
    if popup_height < 3 {
        return;
    }
    let area = Rect::new(
        composer_area.x,
        composer_area.y.saturating_sub(popup_height),
        composer_area.width,
        popup_height,
    );
    render_candidates(
        frame,
        area,
        app,
        &candidates,
        app.slash_completion_index,
        hint.as_deref(),
        None,
    );
}

fn render_candidates(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &AppState,
    candidates: &[super::completion::Candidate],
    index: usize,
    hint: Option<&str>,
    query: Option<&str>,
) {
    let selected = index.min(candidates.len().saturating_sub(1));
    let visible = area.height.saturating_sub(2) as usize;
    let start = selected.saturating_add(1).saturating_sub(visible);
    let items: Vec<_> = if candidates.is_empty() {
        vec![
            ListItem::new(hint.unwrap_or("No results / 没有匹配结果").to_string())
                .style(Style::default().fg(MUTED)),
        ]
    } else {
        candidates
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(i, candidate)| {
                let style = if i == selected {
                    Style::default()
                        .fg(TEXT)
                        .bg(GLASS)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(TEXT).bg(RAISED)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        if i == selected { " › " } else { "   " },
                        Style::default().fg(AMBER),
                    ),
                    Span::styled(format!("{}  ", candidate.name), style),
                    Span::styled(
                        format!(
                            "[{}] {}",
                            super::completion::kind_label(candidate.kind, app.language),
                            candidate.description
                        ),
                        Style::default().fg(if i == selected { TEXT } else { MUTED }),
                    ),
                ]))
                .style(style)
            })
            .collect()
    };
    let title = if let Some(query) = query {
        format!(
            " {} › {} ",
            tr(app.language, TextKey::CommandPalette),
            query
        )
    } else {
        format!(
            " {} · {}/{} ",
            tr(app.language, TextKey::Command),
            if candidates.is_empty() {
                0
            } else {
                selected + 1
            },
            candidates.len()
        )
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title(title)
                .title_bottom(match app.language {
                    Language::ZhCn => " ↑↓ 选择 · Tab 补全 · Esc 关闭 ",
                    Language::En => " ↑↓ select · Tab complete · Esc close ",
                })
                .borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED))
                .style(Style::default().bg(RAISED)),
        ),
        area,
    );
}

pub use super::tool_panel::render as tools;

pub fn footer(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    super::progress::render(frame, area, app);
    let area = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
    let text = if app.armed_session_delete.is_some() {
        match app.language {
            super::i18n::Language::ZhCn => "再次按 d 永久删除 · Esc 取消".to_string(),
            super::i18n::Language::En => {
                "Press d again to delete permanently · Esc cancels".to_string()
            }
        }
    } else if app.focus == Focus::Tools {
        tr(app.language, TextKey::ToolControls).to_string()
    } else if app.focus == Focus::Chat {
        if app.is_busy() {
            match app.language {
                Language::ZhCn => "Ctrl+C 取消 · F2 思考记录 · Tab 工具".into(),
                Language::En => "Ctrl+C stop · F2 reasoning · Tab tools".into(),
            }
        } else if app
            .messages
            .iter()
            .any(|message| message.role == MessageRole::Reasoning)
        {
            match app.language {
                Language::ZhCn => "Enter 发送 · Alt+Enter 换行 · F2 思考记录".into(),
                Language::En => "Enter send · Alt+Enter newline · F2 reasoning".into(),
            }
        } else {
            match app.language {
                Language::ZhCn => "Enter 发送 · Alt+Enter 换行 · Tab 面板 · Ctrl+K 命令".into(),
                Language::En => {
                    "Enter send · Alt+Enter newline · Tab panels · Ctrl+K commands".into()
                }
            }
        }
    } else {
        format!(
            "Tab:{} · Ctrl+K:{} · Ctrl+C:{} · Ctrl+F:{}",
            tr(app.language, TextKey::SwitchPanel),
            tr(app.language, TextKey::Command),
            tr(app.language, TextKey::Cancel),
            tr(app.language, TextKey::Search),
        )
    };
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .style(Style::default().fg(MUTED).bg(GLASS)),
        area,
    );
}

fn compact_id(id: &str) -> String {
    if id.chars().count() > 12 {
        format!("{}…", id.chars().take(11).collect::<String>())
    } else {
        id.to_string()
    }
}

pub fn command_palette(frame: &mut Frame<'_>, app: &AppState) {
    let candidates = app.command_candidates(
        app.palette_query.as_str().trim_start_matches('/'),
        0..app.composer.as_str().len(),
    );
    let area = centered_rect(frame.area(), 90, 14);
    render_candidates(
        frame,
        area,
        app,
        &candidates,
        app.palette_index,
        None,
        Some(app.palette_query.as_str()),
    );
}

pub fn search_overlay(frame: &mut Frame<'_>, app: &AppState) {
    let area = centered_rect(frame.area(), 60, 5);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(format!(" {} ", tr(app.language, TextKey::Search)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BLUE));
    let inner = block.inner(area);
    let (text, cursor) = app
        .search
        .viewport(inner.width.saturating_sub(2) as usize, false);
    frame.render_widget(block.style(Style::default().bg(RAISED)), area);
    frame.render_widget(
        Paragraph::new(format!("/ {text}")).style(Style::default().fg(TEXT).bg(RAISED)),
        inner,
    );
    if inner.width > 2 && inner.height > 0 && app.approval.is_none() {
        frame.set_cursor_position((inner.x + 2 + cursor as u16, inner.y));
    }
}

pub fn approval(frame: &mut Frame<'_>, app: &AppState) {
    let Some(approval) = &app.approval else {
        return;
    };
    let area = centered_rect(frame.area(), 65, 9);
    frame.render_widget(Clear, area);
    let text = vec![
        Line::styled(
            approval.tool.clone(),
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::styled(approval.summary.clone(), Style::default().fg(TEXT)),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                format!("[Y] {}", tr(app.language, TextKey::Approve)),
                Style::default().fg(BLUE),
            ),
            Span::raw("    "),
            Span::styled(
                format!("[N] {}", tr(app.language, TextKey::Deny)),
                Style::default().fg(ERROR),
            ),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(AMBER)),
            )
            .style(Style::default().bg(RAISED)),
        area,
    );
}

pub fn too_small(frame: &mut Frame<'_>, app: &AppState) {
    frame.render_widget(
        Paragraph::new(format!(
            "{}\n\nCtrl+C: {}",
            tr(app.language, TextKey::Resize),
            tr(app.language, TextKey::Exit)
        ))
        .alignment(Alignment::Center)
        .style(Style::default().fg(AMBER).bg(BG)),
        frame.area(),
    );
}

pub(super) fn panel_style(active: bool) -> Style {
    Style::default()
        .fg(if active { TEXT } else { MUTED })
        .bg(if active { RAISED } else { BG })
}

fn centered_rect(area: Rect, percent_x: u16, height: u16) -> Rect {
    let horizontal = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(area);
    let available = horizontal[1];
    let top = available.height.saturating_sub(height) / 2;
    Rect::new(
        available.x,
        available.y + top,
        available.width,
        height.min(available.height),
    )
}

#[cfg(test)]
mod recovery_label_tests {
    use super::*;

    #[test]
    fn overflow_compaction_triggers_are_localized() {
        assert_eq!(
            compaction_trigger_label("preflight_overflow", Language::En),
            "tool-output overflow"
        );
        assert_eq!(
            compaction_trigger_label("provider_overflow", Language::En),
            "provider overflow recovery"
        );
        assert_eq!(
            compaction_trigger_label("preflight_overflow", Language::ZhCn),
            "工具输出溢出"
        );
        assert_eq!(
            compaction_trigger_label("provider_overflow", Language::ZhCn),
            "服务端上下文溢出恢复"
        );
    }
}

fn wrap_transcript(lines: Vec<Line<'_>>, width: usize) -> Vec<Line<'static>> {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let mut output = Vec::new();
    for line in lines {
        let mut spans = Vec::new();
        let mut columns = 0;
        for span in line.spans {
            for grapheme in span.content.graphemes(true) {
                if grapheme == "\n" || grapheme == "\r\n" {
                    output.push(Line::from(std::mem::take(&mut spans)));
                    columns = 0;
                    continue;
                }
                let text = if grapheme == "\t" { "    " } else { grapheme };
                if text.chars().any(char::is_control) {
                    continue;
                }
                let size = text.width();
                if columns + size > width && !spans.is_empty() {
                    output.push(Line::from(std::mem::take(&mut spans)));
                    columns = 0;
                }
                spans.push(Span::styled(text.to_string(), line.style.patch(span.style)));
                columns += size;
            }
        }
        output.push(Line::from(spans));
    }
    output
}
