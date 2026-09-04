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
