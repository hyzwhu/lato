use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InputBuffer {
    text: String,
    cursor: usize,
}

impl InputBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub fn from(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.len();
        Self { text, cursor }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Byte offset of the insertion cursor.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replace a byte range, expanding its edges to whole graphemes.
    /// An empty range inserts at the preceding grapheme boundary.
    pub fn replace_range(&mut self, range: std::ops::Range<usize>, value: &str) {
        let requested_start = range.start.min(self.text.len());
        let requested_end = range.end.min(self.text.len()).max(requested_start);
        let boundaries = self
            .text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .chain(std::iter::once(self.text.len()))
            .collect::<Vec<_>>();
        let start = boundaries
            .iter()
            .copied()
            .take_while(|i| *i <= requested_start)
            .last()
            .unwrap_or(0);
        let end = if requested_start == requested_end {
            start
        } else {
            boundaries
                .iter()
                .copied()
                .find(|i| *i >= requested_end)
                .unwrap_or(self.text.len())
        };
        self.text.replace_range(start..end, value);
        let desired = start + value.len();
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|i| *i >= desired)
            .unwrap_or(self.text.len());
    }

    /// Wrapped display lines and the cursor's (column, row) within the viewport.
    pub fn multiline_viewport(&self, width: usize, height: usize) -> (Vec<String>, usize, usize) {
        if width == 0 || height == 0 {
            return (Vec::new(), 0, 0);
        }
        let (lines, column, row) = self.wrapped_lines(width);
        let start = (row + 1).saturating_sub(height);
        (
            lines.into_iter().skip(start).take(height).collect(),
            column,
            row - start,
        )
    }

    pub fn visual_line_count(&self, width: usize) -> usize {
        if width == 0 {
            return 0;
        }
        self.wrapped_lines(width).0.len()
    }

    fn wrapped_lines(&self, width: usize) -> (Vec<String>, usize, usize) {
        let mut lines = vec![String::new()];
        let mut column = 0;
        let mut cursor = (0, 0);
        let mut just_wrapped = false;
        for (index, grapheme) in self.text.grapheme_indices(true) {
            let newline = grapheme.contains('\n');
            let shown = grapheme
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect::<String>();
            let size = UnicodeWidthStr::width(shown.as_str());
            if !newline && column + size > width && column > 0 {
                lines.push(String::new());
                column = 0;
            }
            if index == self.cursor {
                cursor = (column, lines.len() - 1);
            }
            if newline {
                if !just_wrapped {
                    lines.push(String::new());
                }
                column = 0;
                just_wrapped = false;
                continue;
            }
            // A wide grapheme cannot fit a one-cell terminal. Show a single
            // replacement cell rather than overflow into the adjoining panel.
            if size > width {
                lines.last_mut().unwrap().push('�');
                column += 1;
            } else {
                lines.last_mut().unwrap().push_str(&shown);
                column += size;
            }
            just_wrapped = column >= width;
            if just_wrapped {
                lines.push(String::new());
                column = 0;
            }
        }
        if self.cursor == self.text.len() {
            cursor = (column, lines.len() - 1);
        }
        (lines, cursor.0, cursor.1)
    }

    /// Move between logical lines using terminal columns. A false return means
    /// the cursor is already at the edge, allowing the caller to browse history.
    pub fn move_vertical(&mut self, delta: i32) -> bool {
        if delta == 0 {
            return false;
        }
        let starts = std::iter::once(0)
            .chain(self.text.match_indices('\n').map(|(i, _)| i + 1))
            .collect::<Vec<_>>();
        let current = starts
            .partition_point(|start| *start <= self.cursor)
            .saturating_sub(1);
        let target = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            current.saturating_add(delta as usize).min(starts.len() - 1)
        };
        if target == current {
            return false;
        }
        let desired = UnicodeWidthStr::width(&self.text[starts[current]..self.cursor]);
        let start = starts[target];
        let mut end = self.text[start..]
            .find('\n')
            .map_or(self.text.len(), |i| start + i);
        if end > start && self.text.as_bytes()[end - 1] == b'\r' {
            end -= 1;
        }
        let mut column = 0;
        self.cursor = start;
        for (index, grapheme) in self.text[start..end].grapheme_indices(true) {
            let size = UnicodeWidthStr::width(grapheme);
            if column + size > desired {
                break;
            }
            column += size;
            self.cursor = start + index + grapheme.len();
        }
        true
    }

    pub fn move_line_home(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
    }

    pub fn move_line_end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i);
        if self.cursor > 0 && self.text.as_bytes()[self.cursor - 1] == b'\r' {
            self.cursor -= 1;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// A single visible line, with one cell reserved for the insertion cursor.
    pub fn viewport(&self, width: usize, masked: bool) -> (String, usize) {
        if width == 0 {
            return (String::new(), 0);
        }
        let cells = self
            .text
            .grapheme_indices(true)
            .map(|(index, value)| {
                let shown = if masked {
                    "•".to_string()
                } else {
                    value
                        .chars()
                        .map(|c| if c.is_control() { ' ' } else { c })
                        .collect()
                };
                let size = UnicodeWidthStr::width(shown.as_str());
                (index, shown, size)
            })
            .collect::<Vec<_>>();
        let cursor = cells
            .iter()
            .take_while(|(index, _, _)| *index < self.cursor)
            .count();
        let mut start = cursor;
        let mut column = 0;
        while start > 0 && column + cells[start - 1].2 < width {
            start -= 1;
            column += cells[start].2;
        }
        let mut text = String::new();
        let mut used = 0;
        for (_, shown, size) in &cells[start..] {
            if used + size > width {
                break;
            }
            text.push_str(shown);
            used += size;
        }
        (text, column)
    }

    #[cfg(test)]
    pub fn cursor_width(&self) -> usize {
        UnicodeWidthStr::width(&self.text[..self.cursor])
    }

    #[cfg(test)]
    pub fn display_width(&self) -> usize {
        UnicodeWidthStr::width(self.text.as_str())
    }

    pub fn insert_char(&mut self, value: char) {
        self.text.insert(self.cursor, value);
        self.cursor += value.len_utf8();
    }

    pub fn insert_str(&mut self, value: &str) {
        self.text.insert_str(self.cursor, value);
        self.cursor += value.len();
    }

    pub fn replace(&mut self, value: &str) {
        self.text.clear();
        self.text.push_str(value);
        self.cursor = self.text.len();
    }

    pub fn backspace(&mut self) {
        let Some(previous) = self.previous_boundary() else {
            return;
        };
        self.text.drain(previous..self.cursor);
        self.cursor = previous;
    }

    pub fn delete(&mut self) {
        let Some(next) = self.next_boundary() else {
            return;
        };
        self.text.drain(self.cursor..next);
    }

    pub fn move_left(&mut self) {
        if let Some(previous) = self.previous_boundary() {
            self.cursor = previous;
        }
    }

    pub fn move_right(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.cursor = next;
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    pub fn clear(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    fn previous_boundary(&self) -> Option<usize> {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(index, _)| index)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.text[self.cursor..]
            .grapheme_indices(true)
            .nth(1)
            .map(|(index, _)| self.cursor + index)
            .or_else(|| (self.cursor < self.text.len()).then_some(self.text.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::InputBuffer;

    #[test]
    fn multiline_viewport_wraps_graphemes_and_follows_cursor() {
        let mut input = InputBuffer::from("ab中e\u{301}🙂\nlast");
        assert_eq!(
            input.multiline_viewport(4, 2),
            (vec!["last".into(), "".into()], 0, 1)
        );
        assert_eq!(input.visual_line_count(4), 4);
        input.move_home();
        assert_eq!(
            input.multiline_viewport(4, 2),
            (vec!["ab中".into(), "e\u{301}🙂".into()], 0, 0)
        );
        input.move_right();
        input.move_right();
        input.move_right();
        assert_eq!(
            input.multiline_viewport(4, 1),
            (vec!["e\u{301}🙂".into()], 0, 0)
        );
    }

    #[test]
    fn multiline_viewport_handles_empty_tiny_and_newline_inputs() {
        assert_eq!(
            InputBuffer::new().multiline_viewport(4, 2),
            (vec!["".into()], 0, 0)
        );
        assert_eq!(
            InputBuffer::from("中").multiline_viewport(1, 2),
            (vec!["�".into(), "".into()], 0, 1)
        );
        assert_eq!(
            InputBuffer::from("abc\n\n").multiline_viewport(3, 4),
            (vec!["abc".into(), "".into(), "".into()], 0, 2)
        );
        assert_eq!(
            InputBuffer::from("a\r\nb").multiline_viewport(3, 3),
            (vec!["a".into(), "b".into()], 1, 1)
        );
        assert_eq!(
            InputBuffer::from("abc").multiline_viewport(0, 2),
            (vec![], 0, 0)
        );
        assert_eq!(
            InputBuffer::from("abc").multiline_viewport(2, 0),
            (vec![], 0, 0)
        );
    }

    #[test]
    fn vertical_movement_uses_display_columns_and_reports_history_edges() {
        let mut input = InputBuffer::from("ab中\n文cd\nx");
        assert!(!input.move_vertical(1));
        assert!(input.move_vertical(-1));
        // Column one is inside 文, so stop at its leading boundary.
        assert_eq!(input.cursor(), "ab中\n".len());
        input.move_right();
        assert!(input.move_vertical(-1));
        assert_eq!(input.cursor(), 2);
        assert!(!input.move_vertical(-1));
        assert!(input.move_vertical(i32::MAX));
        assert_eq!(input.cursor(), input.as_str().len());
        input.move_line_home();
        assert_eq!(input.cursor(), "ab中\n文cd\n".len());
        input.move_line_end();
        assert_eq!(input.cursor(), input.as_str().len());
    }

    #[test]
    fn line_movement_stays_outside_crlf_grapheme() {
        let mut input = InputBuffer::from("ab\r\ncd");
        input.move_home();
        input.move_line_end();
        assert_eq!(input.cursor(), 2);
        assert!(input.move_vertical(1));
        assert_eq!(input.cursor(), 6);
        assert!(input.move_vertical(-1));
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn replace_range_snaps_unicode_edges_and_places_cursor_after_insert() {
        let mut input = InputBuffer::from("a中e\u{301}🙂z");
        input.replace_range(2..5, "文");
        assert_eq!(input.as_str(), "a文🙂z");
        assert_eq!(input.cursor(), "a文".len());
        input.replace_range(2..2, "!");
        assert_eq!(input.as_str(), "a!文🙂z");
        input.replace_range(usize::MAX..usize::MAX, "end");
        assert_eq!(input.as_str(), "a!文🙂zend");
        assert_eq!(input.cursor(), input.as_str().len());
    }

    #[test]
    fn cursor_and_delete_respect_graphemes() {
        let mut input = InputBuffer::from("中e\u{301}🙂");
        input.move_end();
        input.backspace();
        assert_eq!(input.as_str(), "中e\u{301}");
        assert_eq!(input.display_width(), 3);
        input.backspace();
        assert_eq!(input.as_str(), "中");
    }

    #[test]
    fn cursor_width_tracks_chinese_columns() {
        let mut input = InputBuffer::from("中ab");
        input.move_home();
        input.move_right();
        assert_eq!(input.cursor_width(), 2);
        input.insert_str("文");
        assert_eq!(input.as_str(), "中文ab");
        assert_eq!(input.cursor_width(), 4);
    }
    #[test]
    fn viewport_handles_unicode_scrolling_and_masked_cursor() {
        let mut input = InputBuffer::from("ab中文🙂");
        assert_eq!(input.viewport(5, false), ("文🙂".into(), 4));
        input.move_left();
        assert_eq!(input.viewport(5, false), ("中文".into(), 4));
        input.move_home();
        assert_eq!(input.viewport(5, false), ("ab中".into(), 0));
        input.move_end();
        assert_eq!(input.viewport(4, true), ("•••".into(), 3));
        assert_eq!(input.viewport(0, false), (String::new(), 0));
        assert_eq!(
            InputBuffer::from("a\nb").viewport(8, false),
            ("a b".into(), 3)
        );
    }

    #[test]
    fn replace_resets_text_and_moves_cursor_to_end() {
        let mut input = InputBuffer::from("old");
        input.move_home();
        input.replace("/model");
        input.insert_char('!');
        assert_eq!(input.as_str(), "/model!");
    }
}
