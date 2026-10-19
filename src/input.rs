//! A single-line text field, used by the find and go-to-line bars.

#[derive(Clone, Debug, Default)]
pub struct LineInput {
    pub text: String,
    /// Byte offset of the caret.
    pub cursor: usize,
    /// When set, the whole text is selected and the next keystroke replaces it.
    pub all_selected: bool,
}

impl LineInput {
    pub fn new(text: &str) -> Self {
        Self { text: text.to_string(), cursor: text.len(), all_selected: !text.is_empty() }
    }

    fn take_selection(&mut self) {
        if self.all_selected {
            self.text.clear();
            self.cursor = 0;
            self.all_selected = false;
        }
    }

    /// Inserts `s`; line breaks are dropped since the field is one line.
    pub fn insert(&mut self, s: &str) {
        self.take_selection();
        let s: String = s.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    pub fn backspace(&mut self) {
        if self.all_selected {
            return self.take_selection();
        }
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.text.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.all_selected {
            return self.take_selection();
        }
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        if std::mem::take(&mut self.all_selected) {
            self.cursor = 0;
        } else if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }

    pub fn right(&mut self) {
        if std::mem::take(&mut self.all_selected) {
            self.cursor = self.text.len();
        } else if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    pub fn home(&mut self) {
        self.all_selected = false;
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.all_selected = false;
        self.cursor = self.text.len();
    }

    pub fn select_all(&mut self) {
        self.all_selected = !self.text.is_empty();
        self.cursor = self.text.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_replaces_preselected_text() {
        let mut input = LineInput::new("old");
        input.insert("n");
        assert_eq!(input.text, "n");
    }

    #[test]
    fn edits_respect_multibyte_characters() {
        let mut input = LineInput::new("");
        input.insert("añb");
        input.left();
        input.backspace();
        assert_eq!(input.text, "ab");
        input.home();
        input.right();
        input.insert("é\nx");
        assert_eq!(input.text, "aéxb");
        input.delete();
        assert_eq!(input.text, "aéx");
    }
}
