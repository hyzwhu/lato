//! Presentation of observed generation state and the latest context estimate.
use super::{
    i18n::Language,
    state::{AppState, CompactionUiState, GenerationPhase, ToolStatus},
    widgets::{AMBER, BLUE, ERROR, GLASS, MUTED, TEXT},
};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

pub fn spinner(app: &AppState) -> &'static str {
    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[app.animation_frame % FRAMES.len()]
}

pub fn activity(app: &AppState) -> String {
    let zh = app.language == Language::ZhCn;
    if app.approval.is_some() {
        return if zh {
            "等待授权"
        } else {
            "Approval needed"
        }
        .into();
    }
    if let CompactionUiState::Running { .. } = app.compaction {
        return if zh {
            "压缩上下文"
        } else {
            "Compacting context"
        }
        .into();
    }
    if let Some(tool) = app
        .tools
        .iter()
        .rev()
        .find(|tool| tool.status == ToolStatus::Running)
    {
        let count = app
            .tools
            .iter()
            .filter(|tool| tool.status == ToolStatus::Running)
            .count();
        return format!(
            "{} {}{}",
            if zh { "执行" } else { "Running" },
            tool.name,
            if count > 1 {
                format!(" +{}", count - 1)
            } else {
                String::new()
            }
        );
    }
    match (app.generation_phase, zh) {
        (GenerationPhase::Waiting | GenerationPhase::Tools, true) => "等待模型",
        (GenerationPhase::Waiting | GenerationPhase::Tools, false) => "Waiting for model",
        (GenerationPhase::Thinking, true) => "思考中",
        (GenerationPhase::Thinking, false) => "Thinking",
        (GenerationPhase::Answering, true) => "正在回答",
        (GenerationPhase::Answering, false) => "Writing answer",
        (GenerationPhase::Completed, true) => "已完成",
        (GenerationPhase::Completed, false) => "Completed",
        (GenerationPhase::Cancelled, true) => "已取消",
        (GenerationPhase::Cancelled, false) => "Cancelled",
        (GenerationPhase::Failed, true) => "失败",
        (GenerationPhase::Failed, false) => "Failed",
        (_, true) => "就绪",
        (_, false) => "Ready",
    }
    .into()
}

pub fn status_line(app: &AppState) -> String {
    let elapsed = if let CompactionUiState::Running { started_at, .. } = &app.compaction {
        started_at.elapsed().as_secs()
    } else {
        app.elapsed_seconds
    };
    let icon = if app.is_busy() {
        spinner(app)
    } else if app.generation_phase == GenerationPhase::Failed {
        "!"
    } else {
        "•"
    };
    let time = if app.is_busy() || app.generation_phase != GenerationPhase::Idle {
        format!(" · {elapsed}s")
    } else {
        String::new()
    };
    format!("{icon} {}{time}", activity(app))
}

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if area.height == 0 {
        return;
    }
    let zh = app.language == Language::ZhCn;
    let label = if zh { "上下文" } else { "context" };
    let (used, window, percent) = app
        .context_usage
        .as_ref()
        .map(|usage| {
            (
                Some(usage.estimated_input_tokens),
                usage.context_window,
                usage.utilization_percent,
            )
        })
        .unwrap_or((None, None, None));
    let used = used.map(|n| n.to_string()).unwrap_or_else(|| "—".into());
    let window = window.map(|n| n.to_string()).unwrap_or_else(|| "—".into());
    let percentage = percent.map(|n| format!(" ({n}%)")).unwrap_or_default();
    let color = match percent {
        Some(90..) => ERROR,
        Some(75..) => AMBER,
        _ => BLUE,
    };
    let mut budget = vec![Span::styled(
        format!("{label}: {used} / {window}{percentage}"),
        Style::default().fg(color),
    )];
    if area.width >= 80 {
        if let Some(percent) = percent {
            let filled = (percent.min(100) as usize) / 10;
            budget.push(Span::styled(
                format!("  {}{}", "━".repeat(filled), "─".repeat(10 - filled)),
                Style::default().fg(color),
            ));
        }
        budget.push(Span::styled(
            if zh {
                "  token 估算"
            } else {
                "  estimated tokens"
            },
            Style::default().fg(MUTED),
        ));
        if app.is_busy() {
            budget.push(Span::styled(
                if zh {
                    " · 最近采样"
                } else {
                    " · last sample"
                },
                Style::default().fg(MUTED),
            ));
        }
    } else {
        budget.push(Span::styled(
            if zh { " 估算" } else { " est." },
            Style::default().fg(MUTED),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(budget)).style(Style::default().bg(GLASS)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if area.height > 1 {
        let model = if area.width < 80 {
            app.model.rsplit('/').next().unwrap_or(&app.model)
        } else {
            &app.model
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    status_line(app),
                    Style::default().fg(if app.is_busy() { AMBER } else { TEXT }),
                ),
                Span::styled(format!(" · {model}"), Style::default().fg(MUTED)),
            ]))
            .style(Style::default().bg(GLASS)),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }
}
