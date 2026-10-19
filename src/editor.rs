//! Text commands on a document: typing, deleting, caret movement. Anything that needs
//! glyph positions (vertical movement, clicks) lives in the window layer instead.

use crate::document::{Change, Document, EditKind, Selection};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Indent {
    Tabs,
    Spaces(usize),
}

pub struct Editor {
    pub doc: Document,
    pub sel: Selection,
    pub indent: Indent,
    /// Changes not yet consumed by the highlighter and style cache.
    pub changes: Vec<Change>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Space,
    Word,
    Punct,
    Newline,
}

fn class(c: char) -> CharClass {
    if c == '\n' || c == '\r' {
        CharClass::Newline
    } else if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

impl Editor {
    /// `default` is the indentation used when `detect` is off or the file gives no clue.
    pub fn new(doc: Document, default: Indent, detect: bool) -> Self {
        let indent = detect.then(|| detect_indent(&doc)).flatten().unwrap_or(default);
        Self { doc, sel: Selection::default(), indent, changes: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.doc.text.len_chars()
    }

    fn char_at(&self, i: usize) -> Option<char> {
        (i < self.len()).then(|| self.doc.text.char(i))
    }

    pub fn caret_line(&self) -> usize {
        self.doc.text.char_to_line(self.sel.head)
    }

    /// Char index where `line`'s content ends, before its line break.
    pub fn line_end(&self, line: usize) -> usize {
        let text = &self.doc.text;
        if line + 1 >= text.len_lines() {
            return text.len_chars();
        }
        let next = text.line_to_char(line + 1);
        if next >= 2 && text.char(next - 2) == '\r' {
            next - 2
        } else {
            next - 1
        }
    }

    /// The line's text without its line break.
    pub fn line_text(&self, line: usize) -> String {
        let start = self.doc.text.line_to_char(line);
        self.doc.text.slice(start..self.line_end(line)).into()
    }

    pub fn selected_text(&self) -> String {
        self.doc.text.slice(self.sel.start()..self.sel.end()).into()
    }

    /// The selection, or the caret's whole line when nothing is selected.
    pub fn copy(&self) -> String {
        if !self.sel.is_empty() {
            return self.selected_text();
        }
        let line = self.caret_line();
        let start = self.doc.text.line_to_char(line);
        let end = self.line_after(line);
        self.doc.text.slice(start..end).into()
    }

    pub fn cut(&mut self) -> String {
        let text = self.copy();
        if self.sel.is_empty() {
            self.select_line_at(self.sel.head);
        }
        self.delete_selection();
        text
    }

    /// Char index where the next line starts, or the end of the document.
    fn line_after(&self, line: usize) -> usize {
        if line + 1 < self.doc.text.len_lines() {
            self.doc.text.line_to_char(line + 1)
        } else {
            self.len()
        }
    }

    fn next_pos(&self, i: usize) -> usize {
        match self.char_at(i) {
            None => i,
            Some('\r') if self.char_at(i + 1) == Some('\n') => i + 2,
            Some(_) => i + 1,
        }
    }

    fn prev_pos(&self, i: usize) -> usize {
        if i == 0 {
            0
        } else if i >= 2 && self.char_at(i - 1) == Some('\n') && self.char_at(i - 2) == Some('\r') {
            i - 2
        } else {
            i - 1
        }
    }

    fn word_right(&self, mut i: usize) -> usize {
        while self.char_at(i).is_some_and(|c| class(c) == CharClass::Space) {
            i += 1;
        }
        match self.char_at(i).map(class) {
            None => i,
            Some(CharClass::Newline) => self.next_pos(i),
            Some(k) => {
                while self.char_at(i).is_some_and(|c| class(c) == k) {
                    i += 1;
                }
                i
            }
        }
    }

    fn word_left(&self, mut i: usize) -> usize {
        while i > 0 && self.char_at(i - 1).is_some_and(|c| class(c) == CharClass::Space) {
            i -= 1;
        }
        match i.checked_sub(1).and_then(|p| self.char_at(p)).map(class) {
            None => i,
            Some(CharClass::Newline) => self.prev_pos(i),
            Some(k) => {
                while i > 0 && self.char_at(i - 1).is_some_and(|c| class(c) == k) {
                    i -= 1;
                }
                i
            }
        }
    }

    pub fn move_to(&mut self, pos: usize, extend: bool) {
        self.sel.head = pos.min(self.len());
        if !extend {
            self.sel.anchor = self.sel.head;
        }
    }

    pub fn move_left(&mut self, extend: bool) {
        let pos =
            if !extend && !self.sel.is_empty() { self.sel.start() } else { self.prev_pos(self.sel.head) };
        self.move_to(pos, extend);
    }

    pub fn move_right(&mut self, extend: bool) {
        let pos = if !extend && !self.sel.is_empty() { self.sel.end() } else { self.next_pos(self.sel.head) };
        self.move_to(pos, extend);
    }

    pub fn move_word_left(&mut self, extend: bool) {
        self.move_to(self.word_left(self.sel.head), extend);
    }

    pub fn move_word_right(&mut self, extend: bool) {
        self.move_to(self.word_right(self.sel.head), extend);
    }

    /// Smart home: jumps to the first non-blank character, then to column 0.
    pub fn move_line_start(&mut self, extend: bool) {
        let line = self.caret_line();
        let start = self.doc.text.line_to_char(line);
        let indent = self.line_text(line).chars().take_while(|c| *c == ' ' || *c == '\t').count();
        let target = if self.sel.head == start + indent { start } else { start + indent };
        self.move_to(target, extend);
    }

    pub fn move_line_end(&mut self, extend: bool) {
        self.move_to(self.line_end(self.caret_line()), extend);
    }

    pub fn select_all(&mut self) {
        self.sel = Selection { anchor: 0, head: self.len() };
    }

    pub fn select_word_at(&mut self, pos: usize) {
        let k = match self.char_at(pos).map(class) {
            Some(CharClass::Newline) | None => {
                self.sel = Selection::caret(pos);
                return;
            }
            Some(k) => k,
        };
        let (mut start, mut end) = (pos, pos);
        while start > 0 && self.char_at(start - 1).is_some_and(|c| class(c) == k) {
            start -= 1;
        }
        while self.char_at(end).is_some_and(|c| class(c) == k) {
            end += 1;
        }
        self.sel = Selection { anchor: start, head: end };
    }

    pub fn select_line_at(&mut self, pos: usize) {
        let line = self.doc.text.char_to_line(pos);
        let start = self.doc.text.line_to_char(line);
        self.sel = Selection { anchor: start, head: self.line_after(line) };
    }

    fn replace(&mut self, start: usize, end: usize, text: &str, kind: EditKind) {
        if self.doc.read_only {
            return;
        }
        let before = self.sel;
        let after = Selection::caret(start + text.chars().count());
        let changes = self.doc.replace(start, end, text, before, after, kind);
        self.changes.extend(changes);
        self.sel = after;
    }

    pub fn insert_text(&mut self, text: &str) {
        let kind = if text.chars().count() == 1 && text != "\n" { EditKind::Insert } else { EditKind::Other };
        self.replace(self.sel.start(), self.sel.end(), text, kind);
    }

    /// Replaces the selection as its own undo step (find-and-replace, not typing).
    pub fn replace_selection(&mut self, text: &str) {
        self.replace(self.sel.start(), self.sel.end(), text, EditKind::Other);
    }

    /// Applies `(start, end, text)` char-range edits as one undo step.
    pub fn replace_many(&mut self, edits: Vec<(usize, usize, String)>) {
        if self.doc.read_only || edits.is_empty() {
            return;
        }
        let before = self.sel;
        let changes = self.doc.replace_many(edits, before, before);
        self.changes.extend(changes);
        let len = self.len();
        self.sel = Selection::caret(before.head.min(len));
    }

    /// Pastes clipboard text, converting its line endings to the document's.
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n");
        let text = if self.doc.line_ending == "\n" { text } else { text.replace('\n', "\r\n") };
        self.insert_text(&text);
    }

    pub fn newline(&mut self) {
        let line = self.doc.text.char_to_line(self.sel.start());
        let indent: String = self.line_text(line).chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        let text = format!("{}{}", self.doc.line_ending, indent);
        self.replace(self.sel.start(), self.sel.end(), &text, EditKind::Other);
    }

    fn delete_selection(&mut self) -> bool {
        if self.sel.is_empty() {
            return false;
        }
        self.replace(self.sel.start(), self.sel.end(), "", EditKind::Other);
        true
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        let head = self.sel.head;
        // Inside leading space-indentation, step back to the previous indent stop.
        let line = self.caret_line();
        let line_start = self.doc.text.line_to_char(line);
        let col = head - line_start;
        if let Indent::Spaces(width) = self.indent {
            let before: String = self.doc.text.slice(line_start..head).into();
            if col > 0 && before.chars().all(|c| c == ' ') {
                let remove = (col - 1) % width + 1;
                self.replace(head - remove, head, "", EditKind::Delete);
                return;
            }
        }
        self.replace(self.prev_pos(head), head, "", EditKind::Delete);
    }

    pub fn delete_forward(&mut self) {
        if !self.delete_selection() {
            let head = self.sel.head;
            self.replace(head, self.next_pos(head), "", EditKind::Delete);
        }
    }

    pub fn delete_word_left(&mut self) {
        if !self.delete_selection() {
            let head = self.sel.head;
            self.replace(self.word_left(head), head, "", EditKind::Other);
        }
    }

    pub fn delete_word_right(&mut self) {
        if !self.delete_selection() {
            let head = self.sel.head;
            self.replace(head, self.word_right(head), "", EditKind::Other);
        }
    }

    pub fn delete_to_line_start(&mut self) {
        if !self.delete_selection() {
            let head = self.sel.head;
            let start = self.doc.text.line_to_char(self.caret_line());
            self.replace(start, head, "", EditKind::Other);
        }
    }

    fn indent_unit(&self, col: usize) -> String {
        match self.indent {
            Indent::Tabs => "\t".into(),
            Indent::Spaces(width) => " ".repeat(width - col % width),
        }
    }

    pub fn indent(&mut self) {
        let first = self.doc.text.char_to_line(self.sel.start());
        let last = self.doc.text.char_to_line(self.sel.end());
        if first == last {
            let col = self.sel.start() - self.doc.text.line_to_char(first);
            let unit = self.indent_unit(col);
            self.insert_text(&unit);
            return;
        }
        let unit = self.indent_unit(0);
        self.edit_lines(first, last, |line| format!("{unit}{line}"));
    }

    pub fn outdent(&mut self) {
        let first = self.doc.text.char_to_line(self.sel.start());
        let last = self.doc.text.char_to_line(self.sel.end());
        let width = match self.indent {
            Indent::Tabs => 1,
            Indent::Spaces(w) => w,
        };
        self.edit_lines(first, last, |line| {
            let strip = if line.starts_with('\t') {
                1
            } else {
                line.chars().take(width).take_while(|c| *c == ' ').count()
            };
            line[strip..].to_string()
        });
    }

    /// Rewrites each line in `first..=last` as one undo step, keeping the lines selected.
    fn edit_lines(&mut self, first: usize, last: usize, f: impl Fn(&str) -> String) {
        if self.doc.read_only {
            return;
        }
        let start = self.doc.text.line_to_char(first);
        let end = self.line_end(last);
        let old: String = self.doc.text.slice(start..end).into();
        let new: Vec<String> = old.split('\n').map(f).collect();
        let new = new.join("\n");
        if new == old {
            return;
        }
        let before = self.sel;
        let after = Selection { anchor: start, head: start + new.chars().count() };
        let changes = self.doc.replace(start, end, &new, before, after, EditKind::Other);
        self.changes.extend(changes);
        self.sel = after;
    }

    pub fn undo(&mut self) {
        if let Some((sel, changes)) = self.doc.undo() {
            self.sel = sel;
            self.changes.extend(changes);
        }
    }

    pub fn redo(&mut self) {
        if let Some((sel, changes)) = self.doc.redo() {
            self.sel = sel;
            self.changes.extend(changes);
        }
    }
}

/// Guesses tabs vs. spaces (and the space width) from leading whitespace.
fn detect_indent(doc: &Document) -> Option<Indent> {
    let (mut tabs, mut spaces) = (0, 0);
    let mut widths = [0usize; 9];
    for line in doc.text.lines().take(2000) {
        match line.chars().next() {
            Some('\t') => tabs += 1,
            Some(' ') => {
                spaces += 1;
                let n = line.chars().take_while(|c| *c == ' ').count();
                for (w, count) in widths.iter_mut().enumerate().skip(2) {
                    if n % w == 0 {
                        *count += 1;
                    }
                }
            }
            _ => {}
        }
    }
    if tabs > spaces {
        return Some(Indent::Tabs);
    }
    if spaces == 0 {
        return None;
    }
    // The widest width that most indented lines are a multiple of.
    let width = [8, 4, 3, 2].into_iter().find(|&w| widths[w] * 10 >= spaces * 9)?;
    Some(Indent::Spaces(width))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    fn editor(text: &str, caret: usize) -> Editor {
        let mut doc = Document::empty(None);
        doc.text = Rope::from_str(text);
        if text.contains("\r\n") {
            doc.line_ending = "\r\n";
        }
        let mut editor = Editor::new(doc, Indent::Spaces(4), true);
        editor.sel = Selection::caret(caret);
        editor
    }

    fn text(editor: &Editor) -> String {
        editor.doc.text.to_string()
    }

    #[test]
    fn crlf_is_one_step_for_movement_and_backspace() {
        let mut ed = editor("ab\r\ncd", 2);
        ed.move_right(false);
        assert_eq!(ed.sel.head, 4);
        ed.move_left(false);
        assert_eq!(ed.sel.head, 2);
        ed.sel = Selection::caret(4);
        ed.backspace();
        assert_eq!(text(&ed), "abcd");
        assert_eq!(ed.line_end(0), 4);
    }

    #[test]
    fn newline_keeps_indentation_and_line_ending() {
        let mut ed = editor("    foo\r\n", 7);
        ed.newline();
        assert_eq!(text(&ed), "    foo\r\n    \r\n");
        assert_eq!(ed.sel.head, 13);
    }

    #[test]
    fn backspace_in_indentation_goes_to_previous_stop() {
        let mut ed = editor("    \n        x\n", 0);
        ed.indent = Indent::Spaces(4);
        ed.sel = Selection::caret(5 + 6); // six spaces into line 1
        ed.backspace();
        assert_eq!(ed.line_text(1), "      x");
        ed.backspace();
        assert_eq!(ed.line_text(1), "  x");
    }

    #[test]
    fn indent_and_outdent_selected_lines() {
        let mut ed = editor("a\nb\nc", 0);
        ed.indent = Indent::Spaces(2);
        ed.sel = Selection { anchor: 0, head: 3 };
        ed.indent();
        assert_eq!(text(&ed), "  a\n  b\nc");
        ed.outdent();
        assert_eq!(text(&ed), "a\nb\nc");
        ed.undo();
        assert_eq!(text(&ed), "  a\n  b\nc");
    }

    #[test]
    fn word_movement_stops_at_class_changes() {
        let mut ed = editor("let foo_bar = baz();", 0);
        ed.move_word_right(false);
        assert_eq!(ed.sel.head, 3);
        ed.move_word_right(false);
        assert_eq!(ed.sel.head, 11);
        ed.move_word_right(false);
        assert_eq!(ed.sel.head, 13);
        ed.move_word_left(false);
        assert_eq!(ed.sel.head, 12);
        ed.move_word_left(false);
        assert_eq!(ed.sel.head, 4);
    }

    #[test]
    fn copy_and_cut_take_the_whole_line_without_a_selection() {
        let mut ed = editor("one\ntwo\nthree", 5);
        assert_eq!(ed.copy(), "two\n");
        assert_eq!(ed.cut(), "two\n");
        assert_eq!(text(&ed), "one\nthree");
    }

    #[test]
    fn smart_home_toggles_between_indent_and_column_zero() {
        let mut ed = editor("    code", 6);
        ed.move_line_start(false);
        assert_eq!(ed.sel.head, 4);
        ed.move_line_start(false);
        assert_eq!(ed.sel.head, 0);
    }

    #[test]
    fn detects_indentation() {
        assert_eq!(editor("a\n\tb\n\tc\n", 0).indent, Indent::Tabs);
        assert_eq!(editor("a\n  b\n    c\n  d\n", 0).indent, Indent::Spaces(2));
        assert_eq!(editor("a\n    b\n        c\n", 0).indent, Indent::Spaces(4));
        assert_eq!(editor("plain\n", 0).indent, Indent::Spaces(4));
        let doc = |text: &str| {
            let mut doc = Document::empty(None);
            doc.text = Rope::from_str(text);
            doc
        };
        assert_eq!(Editor::new(doc("plain\n"), Indent::Tabs, true).indent, Indent::Tabs);
        assert_eq!(Editor::new(doc("a\n  b\n"), Indent::Tabs, false).indent, Indent::Tabs);
    }

    #[test]
    fn paste_converts_line_endings() {
        let mut ed = editor("x\r\n", 0);
        ed.paste("a\nb");
        assert_eq!(text(&ed), "a\r\nbx\r\n");
    }
}
