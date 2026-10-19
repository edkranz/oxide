//! Text commands on a document: typing, deleting, caret movement. Every command applies
//! to each selection at once (multiple cursors), as a single undo step. Anything that needs
//! glyph positions (vertical movement, clicks) lives in the window layer instead.

use crate::document::{Change, Document, EditKind, Selection};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Indent {
    Tabs,
    Spaces(usize),
}

/// How a language writes comments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comment {
    Line(&'static str),
    Block(&'static str, &'static str),
}

/// Brackets and quotes that typing an opener completes.
pub const ALL_PAIRS: &[(char, char)] =
    &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\''), ('`', '`')];
/// For languages where `'` starts lifetimes or quoted symbols, and for prose.
pub const NO_SINGLE_QUOTE: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('`', '`')];
pub const BRACKETS_ONLY: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}')];

pub struct Editor {
    pub doc: Document,
    /// Sorted by position, non-overlapping, never empty.
    sels: Vec<Selection>,
    /// The selection that is scrolled into view and that single-caret features use.
    primary: usize,
    pub indent: Indent,
    /// Pairs completed while typing; empty turns auto-pairing off.
    pub pairs: &'static [(char, char)],
    /// Changes not yet consumed by the highlighter and style cache.
    pub changes: Vec<Change>,
}

/// One selection's edit: replace `start..end` with `text`, then select `anchor..head`
/// (offsets from `start` in the new text).
struct EditSpec {
    start: usize,
    end: usize,
    text: String,
    anchor: usize,
    head: usize,
}

impl EditSpec {
    /// Replace a range and put the caret after the new text.
    fn replace(start: usize, end: usize, text: String) -> Self {
        let len = text.chars().count();
        Self { start, end, text, anchor: len, head: len }
    }
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

/// Where `pos` ends up after non-overlapping `edits` (sorted by start) are applied. A
/// position inside a replaced range moves to the start of its replacement.
fn map_pos(edits: &[(usize, usize, String)], pos: usize) -> usize {
    let mut shift: isize = 0;
    for (start, end, text) in edits {
        if pos <= *start {
            break;
        }
        if pos < *end {
            return (*start as isize + shift) as usize;
        }
        shift += text.chars().count() as isize - (end - start) as isize;
    }
    (pos as isize + shift) as usize
}

fn leading_whitespace(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

impl Editor {
    /// `default` is the indentation used when `detect` is off or the file gives no clue.
    pub fn new(doc: Document, default: Indent, detect: bool) -> Self {
        let indent = detect.then(|| detect_indent(&doc)).flatten().unwrap_or(default);
        Self {
            doc,
            sels: vec![Selection::default()],
            primary: 0,
            indent,
            pairs: ALL_PAIRS,
            changes: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.doc.text.len_chars()
    }

    fn char_at(&self, i: usize) -> Option<char> {
        (i < self.len()).then(|| self.doc.text.char(i))
    }

    fn char_before(&self, i: usize) -> Option<char> {
        i.checked_sub(1).and_then(|p| self.char_at(p))
    }

    pub fn sels(&self) -> &[Selection] {
        &self.sels
    }

    pub fn primary(&self) -> Selection {
        self.sels[self.primary]
    }

    /// Replaces all selections with one.
    pub fn set_selection(&mut self, sel: Selection) {
        self.set_selections(vec![sel], 0);
    }

    /// Replaces the selections; `primary` indexes into `sels`.
    pub fn set_selections(&mut self, sels: Vec<Selection>, primary: usize) {
        let len = self.len();
        let clamp = |s: Selection| Selection { anchor: s.anchor.min(len), head: s.head.min(len) };
        self.sels = sels.into_iter().map(clamp).collect();
        self.primary = primary.min(self.sels.len().saturating_sub(1));
        if self.sels.is_empty() {
            self.sels.push(Selection::default());
        }
        self.normalize();
        self.doc.seal();
    }

    /// Adds a selection and makes it primary.
    pub fn add_selection(&mut self, sel: Selection) {
        let mut sels = self.sels.clone();
        sels.push(sel);
        let primary = sels.len() - 1;
        self.set_selections(sels, primary);
    }

    /// Moves each selection's head to the matching entry of `heads` (one per selection, in
    /// order), for commands like vertical movement that need glyph positions.
    pub fn set_heads(&mut self, heads: &[usize], extend: bool) {
        let sels = self
            .sels
            .iter()
            .zip(heads)
            .map(|(s, &head)| Selection { anchor: if extend { s.anchor } else { head }, head })
            .collect();
        self.set_selections(sels, self.primary);
    }

    /// Back to a single caret: the primary selection's.
    pub fn collapse_to_primary(&mut self) {
        let primary = self.primary();
        self.set_selection(primary);
    }

    /// Sorts selections and merges any that overlap, keeping track of the primary one.
    fn normalize(&mut self) {
        let primary = self.sels[self.primary];
        let mut sels = std::mem::take(&mut self.sels);
        sels.sort_by_key(|s| (s.start(), s.end()));
        let mut merged: Vec<Selection> = Vec::with_capacity(sels.len());
        let mut primary_index = 0;
        for sel in sels {
            match merged.last_mut() {
                Some(last) if sel.start() < last.end() || sel.start() == last.start() => {
                    let (start, end) = (last.start(), last.end().max(sel.end()));
                    let forward = sel.head >= sel.anchor;
                    *last = if forward {
                        Selection { anchor: start, head: end }
                    } else {
                        Selection { anchor: end, head: start }
                    };
                }
                _ => merged.push(sel),
            }
            if sel == primary {
                primary_index = merged.len() - 1;
            }
        }
        self.sels = merged;
        self.primary = primary_index;
    }

    pub fn caret_line(&self) -> usize {
        self.doc.text.char_to_line(self.primary().head)
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

    fn text_of(&self, sel: Selection) -> String {
        self.doc.text.slice(sel.start()..sel.end()).into()
    }

    /// The primary selection's text.
    pub fn selected_text(&self) -> String {
        self.text_of(self.primary())
    }

    /// Char index where the next line starts, or the end of the document.
    fn line_after(&self, line: usize) -> usize {
        if line + 1 < self.doc.text.len_lines() {
            self.doc.text.line_to_char(line + 1)
        } else {
            self.len()
        }
    }

    /// The selected text, one selection per line. With no selection at all, whole lines.
    pub fn copy(&self) -> String {
        if self.sels.iter().all(Selection::is_empty) {
            let mut lines: Vec<usize> =
                self.sels.iter().map(|s| self.doc.text.char_to_line(s.head)).collect();
            lines.dedup();
            return lines
                .into_iter()
                .map(|l| String::from(self.doc.text.slice(self.doc.text.line_to_char(l)..self.line_after(l))))
                .collect();
        }
        let parts: Vec<String> =
            self.sels.iter().filter(|s| !s.is_empty()).map(|s| self.text_of(*s)).collect();
        parts.join(self.doc.line_ending)
    }

    pub fn cut(&mut self) -> String {
        let text = self.copy();
        if self.sels.iter().all(Selection::is_empty) {
            let lines: Vec<Selection> = self.sels.iter().map(|s| self.line_selection(s.head)).collect();
            self.set_selections(lines, self.primary);
        }
        self.edit_each(EditKind::Other, |_, sel| {
            (!sel.is_empty()).then(|| EditSpec::replace(sel.start(), sel.end(), String::new()))
        });
        text
    }

    /// One step right, treating a grapheme cluster (an emoji, a letter with accents) and
    /// a CRLF line break as one character.
    fn next_pos(&self, i: usize) -> usize {
        match self.char_at(i) {
            None => i,
            Some('\r') if self.char_at(i + 1) == Some('\n') => i + 2,
            Some('\n' | '\r') => i + 1,
            Some(_) => {
                let line = self.doc.text.char_to_line(i);
                let start = self.doc.text.line_to_char(line);
                let text = self.line_text(line);
                let byte = char_to_byte(&text, i - start);
                let grapheme = text[byte..].graphemes(true).next().unwrap_or("");
                i + grapheme.chars().count().max(1)
            }
        }
    }

    fn prev_pos(&self, i: usize) -> usize {
        match self.char_before(i) {
            None => 0,
            Some('\n') if i >= 2 && self.char_at(i - 2) == Some('\r') => i - 2,
            Some('\n' | '\r') => i - 1,
            Some(_) => {
                let line = self.doc.text.char_to_line(i);
                let start = self.doc.text.line_to_char(line);
                let text = self.line_text(line);
                let byte = char_to_byte(&text, i - start);
                let grapheme = text[..byte].graphemes(true).next_back().unwrap_or("");
                i - grapheme.chars().count().max(1)
            }
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
        while self.char_before(i).is_some_and(|c| class(c) == CharClass::Space) {
            i -= 1;
        }
        match self.char_before(i).map(class) {
            None => i,
            Some(CharClass::Newline) => self.prev_pos(i),
            Some(k) => {
                while self.char_before(i).is_some_and(|c| class(c) == k) {
                    i -= 1;
                }
                i
            }
        }
    }

    /// Moves each selection's head to `f(selection)`; extending keeps the anchor.
    fn move_each(&mut self, extend: bool, f: impl Fn(&Self, Selection) -> usize) {
        let sels = self
            .sels
            .iter()
            .map(|&s| {
                let head = f(self, s);
                Selection { anchor: if extend { s.anchor } else { head }, head }
            })
            .collect();
        self.set_selections(sels, self.primary);
    }

    /// A single caret at `pos`, or the primary selection extended to it.
    pub fn move_to(&mut self, pos: usize, extend: bool) {
        let anchor = if extend { self.primary().anchor } else { pos };
        self.set_selection(Selection { anchor, head: pos });
    }

    pub fn move_left(&mut self, extend: bool) {
        self.move_each(
            extend,
            |ed, s| if !extend && !s.is_empty() { s.start() } else { ed.prev_pos(s.head) },
        );
    }

    pub fn move_right(&mut self, extend: bool) {
        self.move_each(extend, |ed, s| if !extend && !s.is_empty() { s.end() } else { ed.next_pos(s.head) });
    }

    pub fn move_word_left(&mut self, extend: bool) {
        self.move_each(extend, |ed, s| ed.word_left(s.head));
    }

    pub fn move_word_right(&mut self, extend: bool) {
        self.move_each(extend, |ed, s| ed.word_right(s.head));
    }

    /// Smart home: jumps to the first non-blank character, then to column 0.
    pub fn move_line_start(&mut self, extend: bool) {
        self.move_each(extend, |ed, s| {
            let line = ed.doc.text.char_to_line(s.head);
            let start = ed.doc.text.line_to_char(line);
            let indent = leading_whitespace(&ed.line_text(line)).chars().count();
            if s.head == start + indent {
                start
            } else {
                start + indent
            }
        });
    }

    pub fn move_line_end(&mut self, extend: bool) {
        self.move_each(extend, |ed, s| ed.line_end(ed.doc.text.char_to_line(s.head)));
    }

    pub fn select_all(&mut self) {
        self.set_selection(Selection { anchor: 0, head: self.len() });
    }

    fn word_at(&self, pos: usize) -> Selection {
        let k = match self.char_at(pos).map(class) {
            Some(CharClass::Newline) | None => return Selection::caret(pos),
            Some(k) => k,
        };
        let (mut start, mut end) = (pos, pos);
        while self.char_before(start).is_some_and(|c| class(c) == k) {
            start -= 1;
        }
        while self.char_at(end).is_some_and(|c| class(c) == k) {
            end += 1;
        }
        Selection { anchor: start, head: end }
    }

    fn line_selection(&self, pos: usize) -> Selection {
        let line = self.doc.text.char_to_line(pos);
        Selection { anchor: self.doc.text.line_to_char(line), head: self.line_after(line) }
    }

    pub fn select_word_at(&mut self, pos: usize) {
        self.set_selection(self.word_at(pos));
    }

    pub fn select_line_at(&mut self, pos: usize) {
        self.set_selection(self.line_selection(pos));
    }

    /// Adds the next occurrence of the primary selection's text as a new selection, or
    /// selects the word under the caret first. Returns false when there is nothing to add.
    pub fn add_next_occurrence(&mut self) -> bool {
        let primary = self.primary();
        if primary.is_empty() {
            let word = self.word_at(primary.head);
            if word.is_empty() {
                return false;
            }
            let mut sels = self.sels.clone();
            sels[self.primary] = word;
            self.set_selections(sels, self.primary);
            return true;
        }
        let needle = self.text_of(primary);
        let text = self.doc.text.to_string();
        let from = self.doc.text.char_to_byte(primary.end());
        let found = text[from..].find(&needle).map(|i| i + from).or_else(|| text.find(&needle));
        let Some(byte) = found else { return false };
        let start = self.doc.text.byte_to_char(byte);
        let sel = Selection { anchor: start, head: start + needle.chars().count() };
        if self.sels.iter().any(|s| s.start() == sel.start()) {
            return false;
        }
        self.add_selection(sel);
        true
    }

    /// Selects every occurrence of the primary selection's text (or the word at the caret).
    pub fn select_all_occurrences(&mut self) {
        let mut primary = self.primary();
        if primary.is_empty() {
            primary = self.word_at(primary.head);
        }
        let needle = self.text_of(primary);
        if needle.is_empty() {
            return;
        }
        let text = self.doc.text.to_string();
        let len = needle.chars().count();
        let sels: Vec<Selection> = text
            .match_indices(&needle)
            .map(|(byte, _)| self.doc.text.byte_to_char(byte))
            .map(|start| Selection { anchor: start, head: start + len })
            .collect();
        let index = sels.iter().position(|s| s.start() == primary.start()).unwrap_or(0);
        self.set_selections(sels, index);
    }

    /// Splits each multi-line selection into one selection per line.
    pub fn split_into_lines(&mut self) {
        let mut sels = Vec::new();
        for sel in &self.sels {
            let (first, last) =
                (self.doc.text.char_to_line(sel.start()), self.doc.text.char_to_line(sel.end()));
            if first == last {
                sels.push(*sel);
                continue;
            }
            for line in first..=last {
                let start = sel.start().max(self.doc.text.line_to_char(line));
                let end = sel.end().min(self.line_end(line));
                if line == last && start == end && line != first {
                    continue; // The selection ended at the start of this line.
                }
                sels.push(Selection { anchor: start, head: end });
            }
        }
        let primary = sels.len().saturating_sub(1);
        self.set_selections(sels, primary);
    }

    /// Applies one edit per selection (from `f`; `None` leaves it alone) as one undo step.
    fn edit_each(&mut self, kind: EditKind, f: impl Fn(&Self, Selection) -> Option<EditSpec>) {
        if self.doc.read_only {
            return;
        }
        let mut specs: Vec<(usize, EditSpec)> =
            self.sels.iter().enumerate().filter_map(|(i, s)| f(self, *s).map(|e| (i, e))).collect();
        specs.sort_by_key(|(_, e)| e.start);
        // Two cursors may reach for the same text (say, backspace at adjacent positions);
        // the first one wins.
        let mut end = 0;
        specs.retain(|(_, e)| {
            let keep = e.start >= end;
            if keep {
                end = e.end;
            }
            keep
        });
        let edits: Vec<(usize, usize, String)> =
            specs.iter().map(|(_, e)| (e.start, e.end, e.text.clone())).collect();

        let mut after = Vec::with_capacity(self.sels.len());
        for (i, sel) in self.sels.iter().enumerate() {
            match specs.iter().position(|(j, _)| *j == i) {
                Some(k) => {
                    let e = &specs[k].1;
                    let start = map_pos(&edits, e.start);
                    after.push(Selection { anchor: start + e.anchor, head: start + e.head });
                }
                None => after
                    .push(Selection { anchor: map_pos(&edits, sel.anchor), head: map_pos(&edits, sel.head) }),
            }
        }
        let before = self.sels.clone();
        let changes = self.doc.edit(edits, &before, &after, kind);
        self.changes.extend(changes);
        self.sels = after;
        self.normalize();
    }

    /// Applies edits at fixed ranges (not tied to selections) as one undo step; the
    /// selections move with the text.
    fn edit_ranges(&mut self, mut edits: Vec<(usize, usize, String)>, kind: EditKind) {
        if self.doc.read_only || edits.is_empty() {
            return;
        }
        edits.sort_by_key(|e| e.0);
        let after: Vec<Selection> = self
            .sels
            .iter()
            .map(|s| Selection { anchor: map_pos(&edits, s.anchor), head: map_pos(&edits, s.head) })
            .collect();
        let before = self.sels.clone();
        let changes = self.doc.edit(edits, &before, &after, kind);
        self.changes.extend(changes);
        self.sels = after;
        self.normalize();
    }

    pub fn insert_text(&mut self, text: &str) {
        let mut chars = text.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if self.type_pair(c) {
                return;
            }
        }
        let typing = text.chars().count() == 1 && self.sels.iter().all(Selection::is_empty);
        let kind = if typing { EditKind::Insert } else { EditKind::Other };
        self.edit_each(kind, |_, s| Some(EditSpec::replace(s.start(), s.end(), text.to_string())));
    }

    /// Auto-pairing for one typed character. Returns true if it handled the keystroke.
    fn type_pair(&mut self, c: char) -> bool {
        let all_empty = self.sels.iter().all(Selection::is_empty);
        // Typing a closer that is already next steps over it.
        let is_closer = self.pairs.iter().any(|&(_, close)| close == c);
        if is_closer && all_empty && self.sels.iter().all(|s| self.char_at(s.head) == Some(c)) {
            self.move_each(false, |_, s| s.head + 1);
            return true;
        }
        let Some(&(open, close)) = self.pairs.iter().find(|&&(open, _)| open == c) else { return false };
        if !all_empty {
            // Wrap each selection, keeping the text inside selected.
            self.edit_each(EditKind::Other, |ed, s| {
                let inner = ed.text_of(s);
                let len = inner.chars().count();
                let (anchor, head) = if s.head >= s.anchor { (1, len + 1) } else { (len + 1, 1) };
                Some(EditSpec {
                    start: s.start(),
                    end: s.end(),
                    text: format!("{open}{inner}{close}"),
                    anchor,
                    head,
                })
            });
            return true;
        }
        let quote = open == close;
        let fits = |s: &Selection| {
            let next_ok = self.char_at(s.head).is_none_or(|n| n.is_whitespace() || ")]};,".contains(n));
            let prev = self.char_before(s.head);
            let prev_ok = !quote || prev.is_none_or(|p| !(p.is_alphanumeric() || p == '_' || p == c));
            next_ok && prev_ok
        };
        if !self.sels.iter().all(fits) {
            return false;
        }
        self.edit_each(EditKind::Other, |_, s| {
            Some(EditSpec { start: s.head, end: s.head, text: format!("{open}{close}"), anchor: 1, head: 1 })
        });
        true
    }

    /// Replaces the primary selection as its own undo step (find and replace).
    pub fn replace_selection(&mut self, text: &str) {
        let s = self.primary();
        self.edit_ranges(vec![(s.start(), s.end(), text.to_string())], EditKind::Other);
    }

    /// Applies `(start, end, text)` char-range edits as one undo step.
    pub fn replace_many(&mut self, edits: Vec<(usize, usize, String)>) {
        self.edit_ranges(edits, EditKind::Other);
    }

    /// Pastes clipboard text, converting its line endings to the document's. If it has
    /// one line per selection, each selection gets its own line.
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n");
        let lines: Vec<&str> = text.strip_suffix('\n').unwrap_or(&text).split('\n').collect();
        let ending = self.doc.line_ending;
        if self.sels.len() > 1 && lines.len() == self.sels.len() {
            let order: Vec<Selection> = self.sels.clone();
            self.edit_each(EditKind::Other, |_, s| {
                let i = order.iter().position(|o| *o == s)?;
                Some(EditSpec::replace(s.start(), s.end(), lines[i].to_string()))
            });
            return;
        }
        let text = if ending == "\n" { text } else { text.replace('\n', ending) };
        self.edit_each(EditKind::Other, |_, s| Some(EditSpec::replace(s.start(), s.end(), text.clone())));
    }

    /// Breaks the line, keeping its indentation; one level deeper after an opening
    /// bracket, and between a bracket pair the closer goes on its own line.
    pub fn newline(&mut self) {
        let unit = self.indent_unit(0);
        self.edit_each(EditKind::Other, |ed, s| {
            let line = ed.doc.text.char_to_line(s.start());
            let indent = leading_whitespace(&ed.line_text(line)).to_string();
            let ending = ed.doc.line_ending;
            let before = ed.char_before(s.start());
            let after = ed.char_at(s.end());
            let opens = before.is_some_and(|b| "([{".contains(b));
            let text = match (opens, after) {
                (true, Some(a)) if BRACKETS_ONLY.contains(&(before.unwrap(), a)) => {
                    let inner = format!("{ending}{indent}{unit}");
                    let len = inner.chars().count();
                    let text = format!("{inner}{ending}{indent}");
                    return Some(EditSpec { start: s.start(), end: s.end(), text, anchor: len, head: len });
                }
                (true, _) => format!("{ending}{indent}{unit}"),
                _ => format!("{ending}{indent}"),
            };
            Some(EditSpec::replace(s.start(), s.end(), text))
        });
    }

    pub fn backspace(&mut self) {
        let width = match self.indent {
            Indent::Spaces(w) => w,
            Indent::Tabs => 0,
        };
        self.edit_each(EditKind::Delete, |ed, s| {
            if !s.is_empty() {
                return Some(EditSpec::replace(s.start(), s.end(), String::new()));
            }
            let head = s.head;
            // An empty pair goes as a whole.
            if let (Some(b), Some(a)) = (ed.char_before(head), ed.char_at(head)) {
                if ed.pairs.contains(&(b, a)) {
                    return Some(EditSpec::replace(head - 1, head + 1, String::new()));
                }
            }
            // Inside leading space-indentation, step back to the previous indent stop.
            let line_start = ed.doc.text.line_to_char(ed.doc.text.char_to_line(head));
            let col = head - line_start;
            let before: String = ed.doc.text.slice(line_start..head).into();
            if width > 0 && col > 0 && before.chars().all(|c| c == ' ') {
                let remove = (col - 1) % width + 1;
                return Some(EditSpec::replace(head - remove, head, String::new()));
            }
            (head > 0).then(|| EditSpec::replace(ed.prev_pos(head), head, String::new()))
        });
    }

    /// Deletes each selection, or for empty ones, from the caret to `target(caret)`.
    fn delete_each(&mut self, kind: EditKind, target: impl Fn(&Self, usize) -> usize) {
        self.edit_each(kind, |ed, s| {
            let (start, end) = if s.is_empty() {
                let t = target(ed, s.head);
                (t.min(s.head), t.max(s.head))
            } else {
                (s.start(), s.end())
            };
            (start != end).then(|| EditSpec::replace(start, end, String::new()))
        });
    }

    pub fn delete_forward(&mut self) {
        self.delete_each(EditKind::Delete, Self::next_pos);
    }

    pub fn delete_word_left(&mut self) {
        self.delete_each(EditKind::Other, Self::word_left);
    }

    pub fn delete_word_right(&mut self) {
        self.delete_each(EditKind::Other, Self::word_right);
    }

    pub fn delete_to_line_start(&mut self) {
        self.delete_each(EditKind::Other, |ed, head| {
            ed.doc.text.line_to_char(ed.doc.text.char_to_line(head))
        });
    }

    fn indent_unit(&self, col: usize) -> String {
        match self.indent {
            Indent::Tabs => "\t".into(),
            Indent::Spaces(width) => " ".repeat(width - col % width),
        }
    }

    /// Every line touched by a selection, ascending. A multi-line selection ending at the
    /// start of a line doesn't include that line.
    fn selected_lines(&self) -> Vec<usize> {
        let mut lines = Vec::new();
        for s in &self.sels {
            let first = self.doc.text.char_to_line(s.start());
            let mut last = self.doc.text.char_to_line(s.end());
            if last > first && s.end() == self.doc.text.line_to_char(last) {
                last -= 1;
            }
            lines.extend(first..=last);
        }
        lines.sort_unstable();
        lines.dedup();
        lines
    }

    pub fn indent(&mut self) {
        let single_lines = self
            .sels
            .iter()
            .all(|s| self.doc.text.char_to_line(s.start()) == self.doc.text.char_to_line(s.end()));
        if single_lines {
            self.edit_each(EditKind::Other, |ed, s| {
                let col = s.start() - ed.doc.text.line_to_char(ed.doc.text.char_to_line(s.start()));
                Some(EditSpec::replace(s.start(), s.end(), ed.indent_unit(col)))
            });
            return;
        }
        let unit = self.indent_unit(0);
        let edits = self
            .selected_lines()
            .into_iter()
            .filter(|&l| !self.line_text(l).trim().is_empty())
            .map(|l| {
                let start = self.doc.text.line_to_char(l);
                (start, start, unit.clone())
            })
            .collect();
        self.edit_ranges(edits, EditKind::Other);
    }

    pub fn outdent(&mut self) {
        let width = match self.indent {
            Indent::Tabs => 1,
            Indent::Spaces(w) => w,
        };
        let edits = self
            .selected_lines()
            .into_iter()
            .filter_map(|l| {
                let text = self.line_text(l);
                let strip = if text.starts_with('\t') {
                    1
                } else {
                    text.chars().take(width).take_while(|c| *c == ' ').count()
                };
                let start = self.doc.text.line_to_char(l);
                (strip > 0).then(|| (start, start + strip, String::new()))
            })
            .collect();
        self.edit_ranges(edits, EditKind::Other);
    }

    /// Comments or uncomments the selected lines (line comments), or wraps / unwraps
    /// each selection (block-comment languages).
    pub fn toggle_comment(&mut self, style: Comment) {
        match style {
            Comment::Line(token) => self.toggle_line_comment(token),
            Comment::Block(open, close) => self.toggle_block_comment(open, close),
        }
    }

    fn toggle_line_comment(&mut self, token: &str) {
        let lines: Vec<(usize, String)> = self
            .selected_lines()
            .into_iter()
            .map(|l| (l, self.line_text(l)))
            .filter(|(_, text)| !text.trim().is_empty())
            .collect();
        if lines.is_empty() {
            return;
        }
        let commented = lines.iter().all(|(_, text)| text.trim_start().starts_with(token));
        let column =
            lines.iter().map(|(_, text)| leading_whitespace(text).chars().count()).min().unwrap_or(0);
        let edits = lines
            .iter()
            .map(|(l, text)| {
                let start = self.doc.text.line_to_char(*l);
                if commented {
                    let at = start + leading_whitespace(text).chars().count();
                    let rest = &text.trim_start()[token.len()..];
                    let len = token.chars().count() + usize::from(rest.starts_with(' '));
                    (at, at + len, String::new())
                } else {
                    (start + column, start + column, format!("{token} "))
                }
            })
            .collect();
        self.edit_ranges(edits, EditKind::Other);
    }

    fn toggle_block_comment(&mut self, open: &str, close: &str) {
        self.edit_each(EditKind::Other, |ed, s| {
            // With no selection, the caret's line (without its indentation) is the target.
            let (start, end) = if s.is_empty() {
                let line = ed.doc.text.char_to_line(s.head);
                let line_start = ed.doc.text.line_to_char(line);
                (line_start + leading_whitespace(&ed.line_text(line)).chars().count(), ed.line_end(line))
            } else {
                (s.start(), s.end())
            };
            let text: String = ed.doc.text.slice(start..end).into();
            let replaced = match text.trim().strip_prefix(open).and_then(|t| t.strip_suffix(close)) {
                Some(inner) => inner.trim().to_string(),
                None => format!("{open} {text} {close}"),
            };
            let len = replaced.chars().count();
            Some(EditSpec { start, end, text: replaced, anchor: 0, head: len })
        });
    }

    pub fn undo(&mut self) {
        if let Some((sels, changes)) = self.doc.undo() {
            self.changes.extend(changes);
            let primary = sels.len().saturating_sub(1);
            self.sels = sels;
            self.primary = primary;
            self.normalize();
        }
    }

    pub fn redo(&mut self) {
        if let Some((sels, changes)) = self.doc.redo() {
            self.changes.extend(changes);
            let primary = sels.len().saturating_sub(1);
            self.sels = sels;
            self.primary = primary;
            self.normalize();
        }
    }
}

/// The comment style for a syntax, by its name as the highlighter reports it.
pub fn comment_style(syntax: &str) -> Option<Comment> {
    Some(match syntax {
        "Rust" | "C" | "C++" | "C#" | "Java" | "JavaScript" | "JavaScript (Babel)" | "TypeScript"
        | "TypeScriptReact" | "Go" | "Swift" | "Kotlin" | "Scala" | "PHP" | "Objective-C"
        | "Objective-C++" | "D" | "Groovy" | "Dart" | "Zig" | "ActionScript" | "Pascal"
        | "Graphviz (DOT)" | "JSON" | "SCSS" | "Sass" | "Less" => Comment::Line("//"),
        "Python"
        | "Ruby"
        | "Ruby on Rails"
        | "Bourne Again Shell (bash)"
        | "Shell-Unix-Generic"
        | "Perl"
        | "YAML"
        | "TOML"
        | "R"
        | "Makefile"
        | "Dockerfile"
        | "Nim"
        | "Elixir"
        | "Julia"
        | "CMake"
        | "TCL"
        | "Git Config"
        | "Git Ignore"
        | "Git Attributes"
        | "fish"
        | "PowerShell"
        | "Nix" => Comment::Line("#"),
        "SQL" | "Haskell" | "Lua" | "Ada" | "Elm" | "AppleScript" => Comment::Line("--"),
        "Lisp" | "Clojure" | "Scheme" | "INI" => Comment::Line(";"),
        "LaTeX" | "TeX" | "Erlang" | "Matlab" => Comment::Line("%"),
        "VimL" => Comment::Line("\""),
        "Batch File" => Comment::Line("REM"),
        "HTML" | "XML" | "Markdown" | "MultiMarkdown" | "HTML (Rails)" | "HTML (Erlang)"
        | "Vue Component" | "SVG" => Comment::Block("<!--", "-->"),
        "CSS" => Comment::Block("/*", "*/"),
        "OCaml" => Comment::Block("(*", "*)"),
        _ => return None,
    })
}

/// The pairs to auto-complete for a syntax: no `'` where it isn't a string quote, and
/// only brackets in prose.
pub fn pairs_for(syntax: &str) -> &'static [(char, char)] {
    match syntax {
        "Rust" | "OCaml" | "Lisp" | "Clojure" | "Scheme" | "Haskell" | "Elm" | "VimL" | "LaTeX" | "TeX" => {
            NO_SINGLE_QUOTE
        }
        "Plain Text" | "Markdown" | "MultiMarkdown" | "reStructuredText" | "Textile" => BRACKETS_ONLY,
        _ => ALL_PAIRS,
    }
}

fn char_to_byte(text: &str, chars: usize) -> usize {
    text.char_indices().nth(chars).map_or(text.len(), |(i, _)| i)
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
                    if n.is_multiple_of(w) {
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
        editor.set_selection(Selection::caret(caret));
        editor
    }

    fn text(editor: &Editor) -> String {
        editor.doc.text.to_string()
    }

    fn heads(editor: &Editor) -> Vec<usize> {
        editor.sels().iter().map(|s| s.head).collect()
    }

    #[test]
    fn crlf_is_one_step_for_movement_and_backspace() {
        let mut ed = editor("ab\r\ncd", 2);
        ed.move_right(false);
        assert_eq!(ed.primary().head, 4);
        ed.move_left(false);
        assert_eq!(ed.primary().head, 2);
        ed.set_selection(Selection::caret(4));
        ed.backspace();
        assert_eq!(text(&ed), "abcd");
        assert_eq!(ed.line_end(0), 4);
    }

    #[test]
    fn movement_and_deletion_treat_graphemes_as_one_character() {
        // "e" + combining acute, then a family emoji made of several code points.
        let s = "e\u{301}👨‍👩‍👧x";
        let mut ed = editor(s, 0);
        ed.move_right(false);
        assert_eq!(ed.primary().head, 2);
        ed.move_right(false);
        assert_eq!(ed.primary().head, 2 + "👨‍👩‍👧".chars().count());
        ed.backspace();
        assert_eq!(text(&ed), "e\u{301}x");
    }

    #[test]
    fn newline_keeps_indentation_and_line_ending() {
        let mut ed = editor("    foo\r\n", 7);
        ed.newline();
        assert_eq!(text(&ed), "    foo\r\n    \r\n");
        assert_eq!(ed.primary().head, 13);
    }

    #[test]
    fn newline_between_brackets_opens_an_indented_line() {
        let mut ed = editor("fn x() {}", 8);
        ed.newline();
        assert_eq!(text(&ed), "fn x() {\n    \n}");
        assert_eq!(ed.primary().head, 13);
    }

    #[test]
    fn backspace_in_indentation_goes_to_previous_stop() {
        let mut ed = editor("    \n        x\n", 0);
        ed.indent = Indent::Spaces(4);
        ed.set_selection(Selection::caret(5 + 6)); // six spaces into line 1
        ed.backspace();
        assert_eq!(ed.line_text(1), "      x");
        ed.backspace();
        assert_eq!(ed.line_text(1), "  x");
    }

    #[test]
    fn indent_and_outdent_selected_lines() {
        let mut ed = editor("a\nb\nc", 0);
        ed.indent = Indent::Spaces(2);
        ed.set_selection(Selection { anchor: 0, head: 3 });
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
        assert_eq!(ed.primary().head, 3);
        ed.move_word_right(false);
        assert_eq!(ed.primary().head, 11);
        ed.move_word_right(false);
        assert_eq!(ed.primary().head, 13);
        ed.move_word_left(false);
        assert_eq!(ed.primary().head, 12);
        ed.move_word_left(false);
        assert_eq!(ed.primary().head, 4);
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
        assert_eq!(ed.primary().head, 4);
        ed.move_line_start(false);
        assert_eq!(ed.primary().head, 0);
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

    #[test]
    fn typing_with_several_cursors_edits_each_and_undoes_together() {
        let mut ed = editor("a\nb\nc", 0);
        ed.set_selections(vec![Selection::caret(0), Selection::caret(2), Selection::caret(4)], 2);
        for c in ["-", " "] {
            ed.insert_text(c);
        }
        assert_eq!(text(&ed), "- a\n- b\n- c");
        assert_eq!(heads(&ed), vec![2, 6, 10]);
        ed.backspace();
        assert_eq!(text(&ed), "-a\n-b\n-c");
        ed.undo();
        assert_eq!(text(&ed), "- a\n- b\n- c");
        ed.undo();
        ed.undo();
        assert_eq!(text(&ed), "a\nb\nc");
    }

    #[test]
    fn add_next_occurrence_selects_the_word_then_its_matches() {
        let mut ed = editor("foo bar foo baz foo", 1);
        assert!(ed.add_next_occurrence());
        assert_eq!(ed.selected_text(), "foo");
        assert!(ed.add_next_occurrence());
        assert!(ed.add_next_occurrence());
        assert!(!ed.add_next_occurrence());
        assert_eq!(ed.sels().len(), 3);
        ed.insert_text("x");
        assert_eq!(text(&ed), "x bar x baz x");
    }

    #[test]
    fn overlapping_cursors_merge() {
        let mut ed = editor("abc", 0);
        ed.set_selections(vec![Selection::caret(1), Selection::caret(2)], 0);
        ed.move_left(false);
        ed.move_left(false);
        assert_eq!(ed.sels(), &[Selection::caret(0)]);
    }

    #[test]
    fn split_into_lines_and_paste_one_line_each() {
        let mut ed = editor("aa\nbb\ncc", 0);
        ed.select_all();
        ed.split_into_lines();
        assert_eq!(ed.sels().len(), 3);
        ed.paste("1\n2\n3\n");
        assert_eq!(text(&ed), "1\n2\n3");
    }

    #[test]
    fn brackets_and_quotes_pair_up() {
        let mut ed = editor("", 0);
        ed.insert_text("(");
        assert_eq!((text(&ed), ed.primary().head), ("()".into(), 1));
        ed.insert_text(")");
        assert_eq!((text(&ed), ed.primary().head), ("()".into(), 2));
        ed.set_selection(Selection::caret(1));
        ed.backspace();
        assert_eq!(text(&ed), "");
        // No pairing right before a word, or for an apostrophe after a letter.
        let mut ed = editor("word", 0);
        ed.insert_text("(");
        assert_eq!(text(&ed), "(word");
        let mut ed = editor("don", 3);
        ed.insert_text("'");
        assert_eq!(text(&ed), "don'");
        // A selection gets wrapped and stays selected.
        let mut ed = editor("x = a + b", 4);
        ed.set_selection(Selection { anchor: 4, head: 9 });
        ed.insert_text("(");
        assert_eq!((text(&ed), ed.selected_text()), ("x = (a + b)".into(), "a + b".into()));
    }

    #[test]
    fn toggle_line_comments_at_the_shared_indent() {
        let mut ed = editor("fn a() {\n    x();\n\n    y();\n}", 9);
        ed.set_selection(Selection { anchor: 9, head: 25 });
        ed.toggle_comment(Comment::Line("//"));
        assert_eq!(text(&ed), "fn a() {\n    // x();\n\n    // y();\n}");
        ed.toggle_comment(Comment::Line("//"));
        assert_eq!(text(&ed), "fn a() {\n    x();\n\n    y();\n}");
    }

    #[test]
    fn toggle_block_comments_around_the_line() {
        let mut ed = editor("  <p>hi</p>", 4);
        ed.toggle_comment(Comment::Block("<!--", "-->"));
        assert_eq!(text(&ed), "  <!-- <p>hi</p> -->");
        ed.toggle_comment(Comment::Block("<!--", "-->"));
        assert_eq!(text(&ed), "  <p>hi</p>");
    }
}
