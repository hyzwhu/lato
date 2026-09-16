//! Dedicated paginated plan review (Phase 8B spec §2, A+ Stage 2).
//!
//! The review is a real widget over the actual terminal content area, not a
//! string stuffed into a generic dialog prompt:
//!
//! * the plan body is wrapped to the *actual* content width and paged by the
//!   *actual* content height, so short terminals paginate more and wide
//!   terminals paginate less — every page always fits the visible area;
//! * the approval control stays disabled until the viewport actually shows
//!   the final display row of the plan (the user scrolled to the real end);
//! * a resize rewraps the content and the approval gate resets: the user
//!   must reach the new bottom again before approving.

use std::path::{Path, PathBuf};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr as _;

/// State of the `/plan approve` review overlay.
#[derive(Debug)]
pub struct PlanReviewState {
    pub plan_path: PathBuf,
    /// The plan text under review (bounded by `PLAN_DRAFT_MAX_BYTES`).
    content: String,
    /// Display rows after wrapping to the current content width.
    rows: Vec<String>,
    /// Index of the first visible display row.
    scroll_row: usize,
    viewport_height: usize,
}

impl PlanReviewState {
    /// Opens the review for `content`. Fails closed on an empty plan: an
    /// empty or missing draft must never reach the approval gate.
    pub fn open(plan_path: &Path, content: &str, width: u16, height: u16) -> Result<Self, String> {
        if content.trim().is_empty() {
            return Err(format!(
                "cannot approve: {} is empty or missing; have the model finish the plan first",
                plan_path.display()
            ));
        }
        let mut state = Self {
            plan_path: plan_path.to_path_buf(),
            content: content.trim_end_matches('\n').to_owned(),
            rows: Vec::new(),
            scroll_row: 0,
            viewport_height: 1,
        };
        state.resize(width, height);
        Ok(state)
    }

    /// Rewraps for a new terminal size and resets the approval gate: the
    /// scroll position is clamped into range but never forced to the new
    /// bottom, so the user must navigate to the end again.
    pub fn resize(&mut self, width: u16, height: u16) {
        let content_width = (width as usize).saturating_sub(6).max(10);
        let content_height = (height as usize).saturating_sub(4).max(1);
        self.rows = wrap_rows_text(&self.content, content_width);
        self.viewport_height = content_height;
        self.scroll_row = self.scroll_row.min(self.max_scroll());
    }

    /// Syncs the viewport height with the rendered content area and clamps
    /// the scroll into range. Called from the render path, where the actual
    /// content height is known.
    pub fn update_viewport(&mut self, height: usize) {
        self.viewport_height = height.max(1);
        self.scroll_row = self.scroll_row.min(self.max_scroll());
    }

    fn max_scroll(&self) -> usize {
        self.rows.len().saturating_sub(self.viewport_height)
    }

    pub fn visible_rows(&self) -> &[String] {
        let start = self.scroll_row.min(self.rows.len());
        let end = (start + self.viewport_height).min(self.rows.len());
        &self.rows[start..end]
    }

    pub fn scroll_down(&mut self, rows: usize) {
        self.scroll_row = (self.scroll_row + rows).min(self.max_scroll());
    }

    pub fn scroll_up(&mut self, rows: usize) {
        self.scroll_row = self.scroll_row.saturating_sub(rows);
    }

    pub fn page_down(&mut self) {
        self.scroll_down(self.viewport_height);
    }

    pub fn page_up(&mut self) {
        self.scroll_up(self.viewport_height);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_row = self.max_scroll();
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_row = 0;
    }

    /// True when the viewport currently displays the final display row of the
    /// plan — i.e. the user has actually arrived at the last row of the last
    /// page.
    pub fn at_end(&self) -> bool {
        !self.rows.is_empty() && self.scroll_row >= self.max_scroll()
    }

    /// The approval gate: enabled only after the user actually reached the
    /// end of the plan in the current (post-resize) layout.
    pub fn can_approve(&self) -> bool {
        self.at_end()
    }

    /// (current page, total page count) by the actual viewport height.
    pub fn page_indicator(&self) -> (usize, usize) {
        let total = self.rows.len().max(1).div_ceil(self.viewport_height.max(1));
        let current = self.scroll_row / self.viewport_height.max(1) + 1;
        (current.min(total), total)
    }
}

/// Wraps plan text into display rows of at most `width` columns. The rows are
/// lossless: every logical line appears in order, split across as many rows
/// as its display width requires; an empty line yields one empty row.
pub fn wrap_rows_text(content: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in content.lines() {
        let line = line.replace('\t', "    ");
        if line.is_empty() {
            rows.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0usize;
        for grapheme in line.graphemes(true) {
            let grapheme_width = grapheme.width();
            if current_width + grapheme_width > width && current_width > 0 {
                rows.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push_str(grapheme);
            current_width += grapheme_width;
        }
        rows.push(current);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_plans_open_with_the_approval_gate_disabled_only_when_scrollable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plan.md");
        // A tiny plan fits entirely in the viewport: the last row is visible
        // immediately, so the gate is open from the start.
        let review = PlanReviewState::open(&path, "# plan\nstep one\n", 100, 40).unwrap();
        assert!(review.can_approve(), "a fully visible plan is reviewable");

        // A long plan in a short viewport starts at the top: the gate is
        // closed until the user reaches the end.
        let long = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut review = PlanReviewState::open(&path, &long, 100, 24).unwrap();
        assert!(!review.can_approve());
        review.scroll_to_bottom();
        assert!(review.can_approve());
    }

    #[test]
    fn empty_plans_fail_closed_at_open() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plan.md");
        for content in ["", "   \n  \n"] {
            let error = PlanReviewState::open(&path, content, 80, 24).unwrap_err();
            assert!(error.contains("cannot approve"), "{error}");
        }
    }

    #[test]
    fn long_plans_paginate_by_actual_viewport_height_without_truncation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plan.md");
        let mut content = String::new();
        for index in 0..4000 {
            content.push_str(&format!("line {index}: filler review content\n"));
        }
        content.push_str("CRITICAL FINAL INSTRUCTION: run only step 9\n");
        assert!(
            content.len() > 4000,
            "plan must exceed the old 4000-char preview"
        );

        // Terminal 80x24 → content width 74, height 20.
        let mut review = PlanReviewState::open(&path, &content, 80, 24).unwrap();
        // Terminal 80x24 → content width 74, viewport height 20.
        let row_total = wrap_rows_text(content.trim_end_matches('\n'), 74).len();
        let (page, total) = review.page_indicator();
        assert_eq!(page, 1);
        assert!(total > 1, "a long plan must span many pages: {total}");
        assert_eq!(total, row_total.div_ceil(20));

        // No truncation: walking every page covers all rows in order (the
        // last step may re-show a few rows of the previous page because the
        // final scroll clamps).
        let mut walked = Vec::new();
        loop {
            walked.extend_from_slice(review.visible_rows());
            if review.at_end() {
                break;
            }
            review.page_down();
        }
        assert!(walked.len() >= row_total);
        let joined = walked.join("\n");
        assert!(joined.contains("CRITICAL FINAL INSTRUCTION"));
        assert!(joined.contains("line 0: filler review content"));
        assert!(joined.contains("line 3999: filler review content"));

        // The approval gate opened exactly when the tail became visible.
        assert!(review.can_approve());
    }

    #[test]
    fn tail_content_is_only_visible_after_scrolling_to_the_end() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plan.md");
        let content = (0..60)
            .map(|i| format!("row {i}"))
            .chain(std::iter::once("CRITICAL FINAL INSTRUCTION".into()))
            .collect::<Vec<_>>()
            .join("\n");
        let mut review = PlanReviewState::open(&path, &content, 100, 24).unwrap();

        // At the top the tail row is not in the visible slice at all.
        assert!(
            !review
                .visible_rows()
                .iter()
                .any(|row| row.contains("CRITICAL"))
        );
        assert!(!review.can_approve());

        // Scrolling to the bottom makes the tail row the last visible row.
        review.scroll_to_bottom();
        let visible = review.visible_rows();
        assert_eq!(
            visible.last().unwrap().as_str(),
            "CRITICAL FINAL INSTRUCTION",
            "the final display row must be the last thing on screen"
        );
        assert!(review.can_approve());
    }

    #[test]
    fn single_line_longer_than_the_width_wraps_into_multiple_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plan.md");
        // One ultra-long line (the old string-based pager treated it as one
        // row and let it run off screen).
        let long_line = format!("START {}", "x".repeat(300));
        let review = PlanReviewState::open(&path, &long_line, 80, 24).unwrap();
        assert!(
            wrap_rows_text(&long_line, 74).len() > 1,
            "a 300-column line must wrap"
        );
        for row in review.visible_rows() {
            assert!(row.width() <= 74, "row must fit the content width: {row:?}");
        }
        // Lossless: the wrapped rows reproduce the source line.
        assert_eq!(review.visible_rows().join(""), long_line);
    }

    #[test]
    fn wide_glyphs_wrap_without_overflowing_the_terminal_row() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plan.md");
        let content = format!("中文计划 {}", "内容".repeat(60));
        let mut review = PlanReviewState::open(&path, &content, 40, 20).unwrap();
        for row in review.visible_rows() {
            assert!(row.width() <= 34, "{row:?}");
        }
        review.scroll_to_bottom();
        for row in review.visible_rows() {
            assert!(row.width() <= 34, "{row:?}");
        }
        assert!(review.can_approve());
    }

    #[test]
    fn resize_rewraps_and_resets_the_approval_gate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plan.md");
        let content = (0..40)
            .map(|i| format!("row {i}"))
            .chain(std::iter::once("CRITICAL FINAL INSTRUCTION".into()))
            .collect::<Vec<_>>()
            .join("\n");
        let mut review = PlanReviewState::open(&path, &content, 100, 24).unwrap();
        review.scroll_to_bottom();
        assert!(review.can_approve());

        // Shrinking the terminal rewraps: the user is no longer at the new
        // bottom, so approval locks again until the end is reached again.
        review.resize(60, 12);
        assert!(!review.can_approve(), "resize must reset the approval gate");
        assert!(review.page_indicator().1 > review.page_indicator().0);
        review.scroll_to_bottom();
        assert!(review.can_approve());

        // Growing the terminal keeps the gate consistent with visibility.
        review.resize(200, 60);
        let fully_visible = wrap_rows_text(content.trim_end_matches('\n'), 194).len() <= 56;
        if fully_visible {
            assert!(review.can_approve());
        } else {
            assert!(!review.can_approve());
            review.scroll_to_bottom();
            assert!(review.can_approve());
        }
    }

    #[test]
    fn escape_style_navigation_never_leaves_residue_and_pages_move_both_ways() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plan.md");
        let content = (0..100)
            .map(|i| format!("row {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut review = PlanReviewState::open(&path, &content, 100, 24).unwrap();
        let first =
            |review: &PlanReviewState| review.visible_rows().first().cloned().unwrap_or_default();
        assert_eq!(first(&review), "row 0");

        review.page_down();
        assert_eq!(first(&review), "row 20");
        review.scroll_down(5);
        assert_eq!(first(&review), "row 25");
        review.page_up();
        assert_eq!(first(&review), "row 5");
        review.scroll_up(50);
        assert_eq!(first(&review), "row 0");

        // Overshooting clamps at the bottom.
        review.scroll_to_bottom();
        review.scroll_down(500);
        assert!(review.at_end());
        review.scroll_to_top();
        review.scroll_up(10);
        assert_eq!(first(&review), "row 0");
    }

    #[test]
    fn wrap_rows_are_lossless_and_bound_the_width() {
        let content = "short\n\na very long line that must wrap at ten columns\n中文宽字符行\n";
        let rows = wrap_rows_text(content, 10);
        for row in &rows {
            assert!(row.width() <= 10, "{row:?}");
        }
        // Lossless across wrap boundaries: concatenating the wrapped rows of
        // one logical line reproduces it exactly.
        assert!(rows.concat().contains("a very long line that must wrap"));
        assert!(
            rows.iter().any(|row| row.is_empty()),
            "empty line preserved"
        );
        // Chinese text wraps by display width, not by char count.
        let wide = wrap_rows_text("一二三四五六七八九十", 6);
        assert!(wide.iter().all(|row| row.width() <= 6));
    }

    // ---- render-buffer level evidence (TestBackend) ------------------------

    fn buffer_text(
        terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
        width: u16,
        height: u16,
    ) -> String {
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

    /// Full render-pipeline evidence at two terminal sizes: the tail line is
    /// NOT in the buffer before the user reaches the end, IS in the buffer
    /// after scrolling to the bottom, and the approval button state changes
    /// from locked to ready exactly then. Also covers a narrow terminal.
    fn assert_tail_and_gate(width: u16, height: u16) {
        use crate::tui::{i18n::Language, render::render, state::AppState};
        use ratatui::{Terminal, backend::TestBackend};
        let directory = tempfile::tempdir().unwrap();
        let plan_path = directory.path().join("plan.md");
        let content = (0..120)
            .map(|index| format!("review line {index}"))
            .chain(std::iter::once(
                "CRITICAL FINAL INSTRUCTION: run only step 9".into(),
            ))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&plan_path, &content).unwrap();

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut app = AppState::new(
            Language::En,
            directory.path().to_path_buf(),
            "provider/model".into(),
            "session".into(),
            vec![],
        );
        app.plan_review = Some(PlanReviewState::open(&plan_path, &content, width, height).unwrap());
        app.overlay = Some(crate::tui::state::Overlay::PlanReview);

        // Before scrolling: the tail is NOT rendered anywhere in the buffer
        // and the approval button is locked.
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let top = buffer_text(&mut terminal, width, height);
        assert!(
            !top.contains("CRITICAL FINAL INSTRUCTION"),
            "tail must not be visible before the user reaches the end: {top}"
        );
        assert!(
            top.contains("Reach the end to enable approval"),
            "approval button must render locked at the top: {top}"
        );
        assert!(!top.contains("Enter: approve"), "{top}");

        // Scrolling to the bottom: the tail IS actually rendered and the
        // approval button is ready.
        app.plan_review.as_mut().unwrap().scroll_to_bottom();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let bottom = buffer_text(&mut terminal, width, height);
        assert!(
            bottom.contains("CRITICAL FINAL INSTRUCTION"),
            "tail must be visible at the bottom of the last page: {bottom}"
        );
        assert!(
            bottom.contains("Enter: approve"),
            "approval button must render ready at the end: {bottom}"
        );
        assert!(!bottom.contains("to enable approval"), "{bottom}");

        // Scrolling back up locks the gate again.
        app.plan_review.as_mut().unwrap().scroll_to_top();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let up = buffer_text(&mut terminal, width, height);
        assert!(
            up.contains("Reach the end to enable approval"),
            "approval must re-lock after scrolling away from the end: {up}"
        );
    }

    #[test]
    fn render_pipeline_shows_tail_and_gate_state_on_standard_terminal() {
        assert_tail_and_gate(100, 30);
    }

    #[test]
    fn render_pipeline_shows_tail_and_gate_state_on_narrow_terminal() {
        assert_tail_and_gate(42, 12);
    }

    #[test]
    fn resize_resets_the_gate_in_the_render_pipeline() {
        use crate::tui::{i18n::Language, render::render, state::AppState};
        use ratatui::{Terminal, backend::TestBackend};
        let directory = tempfile::tempdir().unwrap();
        let plan_path = directory.path().join("plan.md");
        let content = (0..120)
            .map(|index| format!("review line {index}"))
            .chain(std::iter::once(
                "CRITICAL FINAL INSTRUCTION: run only step 9".into(),
            ))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&plan_path, &content).unwrap();

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut app = AppState::new(
            Language::En,
            directory.path().to_path_buf(),
            "provider/model".into(),
            "session".into(),
            vec![],
        );
        app.plan_review = Some(PlanReviewState::open(&plan_path, &content, 100, 30).unwrap());
        app.overlay = Some(crate::tui::state::Overlay::PlanReview);

        // First draw syncs the review's viewport with the real content area.
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        // Reach the end at 100x30: gate open.
        app.plan_review.as_mut().unwrap().scroll_to_bottom();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(
            buffer_text(&mut terminal, 100, 30).contains("Enter: approve"),
            "gate must be open at the end"
        );

        // Simulate a terminal resize through the real reducer path: the gate
        // must lock again and the tail must leave the viewport.
        app.reduce(crate::tui::state::AppEvent::Resize(60, 14));
        let mut shrunk = Terminal::new(TestBackend::new(60, 14)).unwrap();
        shrunk.draw(|frame| render(frame, &mut app)).unwrap();
        let after = buffer_text(&mut shrunk, 60, 14);
        assert!(
            after.contains("Reach the end to enable approval"),
            "resize must re-lock the approval gate: {after}"
        );
        assert!(
            !after.contains("CRITICAL FINAL INSTRUCTION"),
            "tail must not stay visible after shrinking: {after}"
        );

        // Reaching the NEW bottom re-enables approval at the new size.
        app.plan_review.as_mut().unwrap().scroll_to_bottom();
        shrunk.draw(|frame| render(frame, &mut app)).unwrap();
        let reopened = buffer_text(&mut shrunk, 60, 14);
        assert!(
            reopened.contains("Enter: approve") && reopened.contains("CRITICAL FINAL INSTRUCTION"),
            "gate must reopen after reaching the new bottom: {reopened}"
        );
    }
}
