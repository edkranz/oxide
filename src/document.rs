//! The text of one file plus its undo history and on-disk identity.

use ropey::Rope;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Typing within this window of the previous keystroke is undone as one step.
const COALESCE_WINDOW: Duration = Duration::from_millis(1500);

/// A selection in char indices. `head` is where the caret is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn caret(at: usize) -> Self {
        Self { anchor: at, head: at }
    }

    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// How an edit should be grouped with neighbouring edits for undo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    Insert,
    Delete,
    Other,
}

/// Lines `first_line..first_line + old_lines` were replaced by
/// `first_line..first_line + new_lines`.
#[derive(Clone, Copy, Debug)]
pub struct Change {
    pub first_line: usize,
    pub old_lines: usize,
    pub new_lines: usize,
}

struct Edit {
    at: usize,
    removed: String,
    inserted: String,
}

struct Transaction {
    id: u64,
    kind: EditKind,
    edits: Vec<Edit>,
    before: Selection,
    after: Selection,
    time: Instant,
}

pub struct Document {
    pub text: Rope,
    pub path: Option<PathBuf>,
    /// Set when the file was not valid UTF-8; saving would corrupt it.
    pub read_only: bool,
    /// The line ending used for new lines, detected from the file.
    pub line_ending: &'static str,
    /// Bumped on every change to `text`.
    pub version: u64,
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
    next_id: u64,
    saved_id: u64,
}

impl Document {
    pub fn empty(path: Option<PathBuf>) -> Self {
        Self {
            text: Rope::new(),
            path,
            read_only: false,
            line_ending: if cfg!(windows) { "\r\n" } else { "\n" },
            version: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 1,
            saved_id: 0,
        }
    }

    /// Opens `path`, or starts an empty document that will be created on save.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::empty(Some(path))),
            Err(e) => return Err(e),
        };
        let (text, read_only) = match String::from_utf8(bytes) {
            Ok(text) => (text, false),
            Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
        };
        let mut doc = Self::empty(Some(path));
        doc.line_ending = detect_line_ending(&text).unwrap_or(doc.line_ending);
        doc.text = Rope::from_str(&text);
        doc.read_only = read_only;
        Ok(doc)
    }

    pub fn is_dirty(&self) -> bool {
        self.current_id() != self.saved_id
    }

    fn current_id(&self) -> u64 {
        self.undo.last().map_or(0, |t| t.id)
    }

    /// Replaces `start..end` (chars) with `text` as one undoable edit.
    pub fn replace(
        &mut self,
        start: usize,
        end: usize,
        text: &str,
        before: Selection,
        after: Selection,
        kind: EditKind,
    ) -> Vec<Change> {
        if start == end && text.is_empty() {
            return Vec::new();
        }
        let removed: String = self.text.slice(start..end).into();
        let change = self.splice(start, end - start, text);
        let edit = Edit { at: start, removed, inserted: text.to_string() };
        self.redo.clear();

        let now = Instant::now();
        let saved_id = self.saved_id;
        if let Some(last) = self.undo.last_mut() {
            // Never merge into the saved step, or undo would skip past the saved state.
            let mergeable = kind != EditKind::Other
                && last.kind == kind
                && last.id != saved_id
                && now.duration_since(last.time) < COALESCE_WINDOW
                && continues(last.edits.last().unwrap(), &edit, kind);
            if mergeable {
                last.edits.push(edit);
                last.after = after;
                last.time = now;
                return vec![change];
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        self.undo.push(Transaction { id, kind, edits: vec![edit], before, after, time: now });
        vec![change]
    }

    pub fn undo(&mut self) -> Option<(Selection, Vec<Change>)> {
        let tx = self.undo.pop()?;
        let mut changes = Vec::new();
        for edit in tx.edits.iter().rev() {
            let len = edit.inserted.chars().count();
            changes.push(self.splice(edit.at, len, &edit.removed));
        }
        let selection = tx.before;
        self.redo.push(tx);
        // Typing after an undo starts a fresh step.
        if let Some(last) = self.undo.last_mut() {
            last.kind = EditKind::Other;
        }
        Some((selection, changes))
    }

    pub fn redo(&mut self) -> Option<(Selection, Vec<Change>)> {
        let mut tx = self.redo.pop()?;
        let mut changes = Vec::new();
        for edit in &tx.edits {
            let len = edit.removed.chars().count();
            changes.push(self.splice(edit.at, len, &edit.inserted));
        }
        let selection = tx.after;
        // A redone step must never merge with the next keystroke.
        tx.kind = EditKind::Other;
        self.undo.push(tx);
        Some((selection, changes))
    }

    fn splice(&mut self, at: usize, remove: usize, insert: &str) -> Change {
        let first_line = self.text.char_to_line(at);
        let old_lines = self.text.char_to_line(at + remove) - first_line + 1;
        self.text.remove(at..at + remove);
        self.text.insert(at, insert);
        let new_lines = self.text.char_to_line(at + insert.chars().count()) - first_line + 1;
        self.version += 1;
        Change { first_line, old_lines, new_lines }
    }

    /// Writes the file atomically: a temp file in the same directory is renamed over the
    /// target, so a crash mid-save never leaves a truncated file behind.
    pub fn save(&mut self) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::other("file is not valid UTF-8; opened read-only"));
        }
        let path = self
            .path
            .clone()
            .ok_or_else(|| io::Error::other("no file name; open a path with `oxide <file>`"))?;
        // Follow symlinks so the rename replaces the real file, not the link.
        let target = fs::canonicalize(&path).unwrap_or(path);
        let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let tmp = target.with_file_name(format!(".{name}.oxide-save"));

        let result = (|| {
            let mut out = BufWriter::new(fs::File::create(&tmp)?);
            for chunk in self.text.chunks() {
                out.write_all(chunk.as_bytes())?;
            }
            out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
            if let Ok(meta) = fs::metadata(&target) {
                fs::set_permissions(&tmp, meta.permissions())?;
            }
            fs::rename(&tmp, &target)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result?;
        self.saved_id = self.current_id();
        Ok(())
    }
}

/// Whether `next` extends `prev` so the two belong in one undo step.
fn continues(prev: &Edit, next: &Edit, kind: EditKind) -> bool {
    match kind {
        EditKind::Insert => {
            !next.inserted.contains('\n')
                && next.at == prev.at + prev.inserted.chars().count()
        }
        // Backspace walks left; forward-delete stays put.
        EditKind::Delete => {
            next.at + next.removed.chars().count() == prev.at || next.at == prev.at
        }
        EditKind::Other => false,
    }
}

fn detect_line_ending(text: &str) -> Option<&'static str> {
    let i = text.find('\n')?;
    Some(if text[..i].ends_with('\r') { "\r\n" } else { "\n" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_chars(doc: &mut Document, at: usize, text: &str) {
        for (i, c) in text.chars().enumerate() {
            let pos = at + i;
            doc.replace(pos, pos, &c.to_string(), Selection::caret(pos), Selection::caret(pos + 1), EditKind::Insert);
        }
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oxide-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn typing_is_undone_as_one_step() {
        let mut doc = Document::empty(None);
        type_chars(&mut doc, 0, "hello");
        assert_eq!(doc.text.to_string(), "hello");
        let (sel, _) = doc.undo().unwrap();
        assert_eq!(doc.text.to_string(), "");
        assert_eq!(sel, Selection::caret(0));
        doc.redo().unwrap();
        assert_eq!(doc.text.to_string(), "hello");
    }

    #[test]
    fn typing_a_newline_starts_a_new_step() {
        let mut doc = Document::empty(None);
        type_chars(&mut doc, 0, "ab");
        doc.replace(2, 2, "\n", Selection::caret(2), Selection::caret(3), EditKind::Other);
        type_chars(&mut doc, 3, "cd");
        doc.undo();
        assert_eq!(doc.text.to_string(), "ab\n");
        doc.undo();
        assert_eq!(doc.text.to_string(), "ab");
    }

    #[test]
    fn dirty_tracks_the_saved_step_through_undo_and_redo() {
        let dir = scratch_dir("dirty");
        let mut doc = Document::open(dir.join("a.txt")).unwrap();
        assert!(!doc.is_dirty());
        type_chars(&mut doc, 0, "abc");
        assert!(doc.is_dirty());
        doc.save().unwrap();
        assert!(!doc.is_dirty());
        // Typing right after a save must not merge into the saved step.
        type_chars(&mut doc, 3, "d");
        assert!(doc.is_dirty());
        doc.undo();
        assert!(!doc.is_dirty());
        doc.undo();
        assert!(doc.is_dirty());
        doc.redo();
        assert!(!doc.is_dirty());
    }

    #[test]
    fn save_keeps_crlf_and_writes_through_symlinks() {
        let dir = scratch_dir("save");
        let real = dir.join("real.txt");
        fs::write(&real, "one\r\ntwo\r\n").unwrap();
        #[cfg(unix)]
        let path = {
            let link = dir.join("link.txt");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            link
        };
        #[cfg(not(unix))]
        let path = real.clone();

        let mut doc = Document::open(path.clone()).unwrap();
        assert_eq!(doc.line_ending, "\r\n");
        doc.replace(0, 3, "ONE", Selection::caret(0), Selection::caret(3), EditKind::Other);
        doc.save().unwrap();
        assert_eq!(fs::read_to_string(&real).unwrap(), "ONE\r\ntwo\r\n");
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_symlink() || cfg!(not(unix)));
    }

    #[test]
    fn invalid_utf8_opens_read_only() {
        let dir = scratch_dir("utf8");
        let path = dir.join("bin");
        fs::write(&path, [0x66, 0xff, 0x6f]).unwrap();
        let mut doc = Document::open(path.clone()).unwrap();
        assert!(doc.read_only);
        assert!(doc.save().is_err());
        assert_eq!(fs::read(&path).unwrap(), [0x66, 0xff, 0x6f]);
    }

    #[test]
    fn changes_report_replaced_line_ranges() {
        let mut doc = Document::empty(None);
        doc.text = Rope::from_str("a\nb\nc\n");
        // Join lines 1 and 2 ("b\nc" -> "bc").
        let change = doc.replace(3, 4, "", Selection::caret(3), Selection::caret(3), EditKind::Other)[0];
        assert_eq!((change.first_line, change.old_lines, change.new_lines), (1, 2, 1));
        let change = doc.replace(0, 0, "x\ny\n", Selection::caret(0), Selection::caret(4), EditKind::Other)[0];
        assert_eq!((change.first_line, change.old_lines, change.new_lines), (0, 1, 3));
    }
}
