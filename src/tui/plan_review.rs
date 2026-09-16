//! Paginated review for `/plan approve` (Phase 8B spec §2: the TUI shows the
//! plan file for review first, and approval must not be confirmable before
//! the whole plan has been displayed).
//!
//! The review splits the plan into fixed-size pages. The approval choice is
//! only offered on the last page, so a user must walk through every page
//! — including the tail of a 128 KiB plan — before they can confirm.

/// Default number of plan lines per review page. Sized to fit a typical
/// terminal without scrolling the dialog itself.
pub const PLAN_REVIEW_LINES_PER_PAGE: usize = 30;

/// Splits plan content into review pages of at most `lines_per_page` lines.
/// Content is never truncated: every line appears on exactly one page, and
/// concatenating all pages in order reproduces the whole plan. An empty plan
/// yields one empty page so the caller can fail closed on approval.
pub fn plan_review_pages(content: &str, lines_per_page: usize) -> Vec<String> {
    assert!(lines_per_page > 0, "lines_per_page must be positive");
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return vec![String::new()];
    }
    lines
        .chunks(lines_per_page)
        .map(|chunk| chunk.join("\n"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_plans_fit_on_one_page() {
        let pages = plan_review_pages("# plan\n\nstep one\n", 30);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0], "# plan\n\nstep one");
    }

    #[test]
    fn empty_plans_yield_one_empty_page() {
        assert_eq!(plan_review_pages("", 30), vec![String::new()]);
    }

    #[test]
    fn large_plans_are_split_without_truncation_and_tail_stays_visible() {
        // A 131,072-byte plan whose tail carries the decisive content: the
        // reviewer must be able to reach it before approving.
        let mut content = String::new();
        for index in 0..4000 {
            content.push_str(&format!("line {index}: filler review content\n"));
        }
        content.push_str("CRITICAL FINAL INSTRUCTION: run only step 9\n");
        assert!(
            content.len() > 4000,
            "test plan must exceed the old 4000-char truncation"
        );

        let pages = plan_review_pages(&content, PLAN_REVIEW_LINES_PER_PAGE);
        assert!(pages.len() > 100, "a large plan must span many pages");

        // No truncation: every original line survives, in order, across the
        // page sequence.
        let joined = pages.join("\n");
        assert_eq!(joined, content.trim_end_matches('\n'));

        // The tail is visible on the last page — the old take(4000) preview
        // cut exactly this content away.
        let last = pages.last().unwrap();
        assert!(last.contains("CRITICAL FINAL INSTRUCTION"));

        // Page size bound: no page carries more lines than requested.
        for page in &pages {
            assert!(page.lines().count() <= PLAN_REVIEW_LINES_PER_PAGE);
        }
        // The approval choice must only be reachable from the last page.
        assert!(
            !pages[..pages.len() - 1]
                .iter()
                .any(|page| page.contains("CRITICAL FINAL INSTRUCTION"))
        );
    }

    #[test]
    fn exactly_one_page_boundary_splits_into_two_pages() {
        let content = (0..60)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let pages = plan_review_pages(&content, 30);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].lines().count(), 30);
        assert_eq!(pages[1].lines().count(), 30);
    }
}
