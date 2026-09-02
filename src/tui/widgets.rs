use super::{
    i18n::{TextKey, tr},
    state::{AppState, Focus, MessageRole, ToolStatus},
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
            Constraint::Length(3),
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
    let input_area = centered_rect(rows[4], 70, 3);
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

pub fn chat(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let active = app.focus == Focus::Chat;
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(4),
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
            Span::styled(
                format!("{}: —", tr(app.language, TextKey::Token)),
                Style::default().fg(MUTED),
            ),
        ]))
        .style(Style::default().bg(GLASS)),
        rows[0],
    );
    let mut lines = Vec::new();
    for message in &app.messages {
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
                let body = if message.expanded {
                    message.content.clone()
                } else {
                    format!("{}…", message.content.chars().take(42).collect::<String>())
                };
                lines.push(Line::styled(
                    format!("◆ {}  {body}", tr(app.language, TextKey::Thinking)),
                    Style::default().fg(AMBER),
                ));
            }
            MessageRole::System => {
                lines.push(Line::styled(
                    message.content.clone(),
                    Style::default().fg(MUTED),
                ));
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
    frame.render_widget(
        Paragraph::new(lines)
            .style(background)
            .wrap(Wrap { trim: false })
            .scroll((app.scroll, 0)),
        rows[1],
    );
    composer(frame, rows[2], app, false);
}

fn composer(frame: &mut Frame<'_>, area: Rect, app: &AppState, welcome: bool) {
    let rows = Layout::vertical([Constraint::Length(2), Constraint::Length(1)]).split(area);
    let placeholder = if welcome {
        tr(app.language, TextKey::StartPrompt)
    } else {
        tr(app.language, TextKey::MessagePlaceholder)
    };
    let content = if app.composer.is_empty() {
        Span::styled(placeholder, Style::default().fg(MUTED))
    } else {
        Span::styled(app.composer.as_str().to_string(), Style::default().fg(TEXT))
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "› ",
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ),
            content,
        ]))
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(GLASS)),
        )
        .style(Style::default().bg(BG)),
        rows[0],
    );
    let status = if app.responding {
        format!(
            "● {}… {}s                                      [{}]",
            tr(app.language, TextKey::Responding),
            app.elapsed_seconds,
            tr(app.language, TextKey::Stop)
        )
    } else if welcome {
        tr(app.language, TextKey::WelcomeHint).to_string()
    } else {
        tr(app.language, TextKey::Ready).to_string()
    };
    frame.render_widget(
        Paragraph::new(status)
            .alignment(if welcome {
                Alignment::Center
            } else {
                Alignment::Left
            })
            .style(
                Style::default()
                    .fg(if app.responding { AMBER } else { MUTED })
                    .bg(BG),
            ),
        rows[1],
    );
    if app.focus == Focus::Chat && app.overlay.is_none() && app.approval.is_none() {
        let cursor = app.composer.cursor_width() as u16;
        let x = rows[0].x.saturating_add(2).saturating_add(cursor);
        let max_x = rows[0].right().saturating_sub(1);
        frame.set_cursor_position((x.min(max_x), rows[0].y));
    }
}

pub fn tools(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let active = app.focus == Focus::Tools;
    let style = panel_style(active);
    let block = Block::default()
        .title(format!(" {} ", tr(app.language, TextKey::ToolCalls)))
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
                .alignment(Alignment::Center)
                .style(Style::default().fg(MUTED).bg(style.bg.unwrap_or(BG))),
            inner,
        );
        return;
    }
    let mut lines = Vec::new();
    for tool in &app.tools {
        let (symbol, key, color) = match tool.status {
            ToolStatus::Running => ("●", TextKey::Running, AMBER),
            ToolStatus::Done => ("✓", TextKey::Done, BLUE),
            ToolStatus::Error => ("✗", TextKey::Failed, ERROR),
        };
        lines.push(Line::from(vec![
            Span::styled(
                tool.name.clone(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}ms", tool.elapsed_ms),
                Style::default().fg(MUTED),
            ),
        ]));
        lines.push(Line::styled(
            tool.arguments.clone(),
            Style::default().fg(MUTED),
        ));
        lines.push(Line::styled(
            format!("{symbol} {}", tr(app.language, key)),
            Style::default().fg(color),
        ));
        if let Some(result) = &tool.result {
            lines.push(Line::styled(
                format!("{}: {result}", tr(app.language, TextKey::ReturnValue)),
                Style::default().fg(MUTED),
            ));
        }
        lines.push(Line::raw(""));
    }
    frame.render_widget(
        Paragraph::new(lines).style(style).wrap(Wrap { trim: true }),
        inner,
    );
}

pub fn footer(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let text = format!(
        "Tab:{}  |  Cmd/Ctrl+K:{}  |  Ctrl+C:{}  |  /:{}",
        tr(app.language, TextKey::SwitchPanel),
        tr(app.language, TextKey::Command),
        tr(app.language, TextKey::Cancel),
        tr(app.language, TextKey::Search),
    );
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .style(Style::default().fg(MUTED).bg(GLASS)),
        area,
    );
}

pub fn command_palette(frame: &mut Frame<'_>, app: &AppState) {
    let area = centered_rect(frame.area(), 58, 13);
    frame.render_widget(Clear, area);
    let commands = [
        tr(app.language, TextKey::NewSession),
        tr(app.language, TextKey::SwitchSession),
        tr(app.language, TextKey::ClearConversation),
        tr(app.language, TextKey::SwitchModel),
        tr(app.language, TextKey::Login),
        tr(app.language, TextKey::SwitchLanguage),
        tr(app.language, TextKey::Search),
        tr(app.language, TextKey::ApproveOnce),
        tr(app.language, TextKey::Status),
        tr(app.language, TextKey::Exit),
    ];
    let items = commands.iter().enumerate().map(|(index, command)| {
        let style = if index == app.palette_index {
            Style::default()
                .fg(BG)
                .bg(AMBER)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT).bg(RAISED)
        };
        ListItem::new(format!("  {command}")).style(style)
    });
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title(format!(" {} ", tr(app.language, TextKey::CommandPalette)))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED))
                .style(Style::default().bg(RAISED)),
        ),
        area,
    );
}

pub fn search_overlay(frame: &mut Frame<'_>, app: &AppState) {
    let area = centered_rect(frame.area(), 60, 5);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(format!("/ {}", app.search.as_str()))
            .block(
                Block::default()
                    .title(format!(" {} ", tr(app.language, TextKey::Search)))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(BLUE)),
            )
            .style(Style::default().fg(TEXT).bg(RAISED)),
        area,
    );
    frame.set_cursor_position((
        area.x
            .saturating_add(3)
            .saturating_add(app.search.cursor_width() as u16)
            .min(area.right().saturating_sub(2)),
        area.y.saturating_add(2),
    ));
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

fn panel_style(active: bool) -> Style {
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
