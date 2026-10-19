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
    before: Vec<Selection>,
    after: Vec<Selection>,
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

    /// A document restored with unsaved `text`; it stays dirty until saved.
    pub fn with_unsaved(path: Option<PathBuf>, text: String) -> Self {
        let mut doc = Self::empty(path);
        doc.line_ending = detect_line_ending(&text).unwrap_or(doc.line_ending);
        doc.text = Rope::from_str(&text);
        // No undo step will ever have this id, so the document reads as modified.
        doc.saved_id = u64::MAX;
        doc
    }

    pub fn is_dirty(&self) -> bool {
        self.current_id() != self.saved_id
    }

    fn current_id(&self) -> u64 {
        self.undo.last().map_or(0, |t| t.id)
    }

    /// Applies non-overlapping `(start, end, text)` edits (char ranges in the current text)
    /// as one undo step, merged into the previous step when `kind` continues it (typing a
    /// word, or a run of deletions). `before` and `after` are the selections to restore.
    pub fn edit(
        &mut self,
        mut edits: Vec<(usize, usize, String)>,
        before: &[Selection],
        after: &[Selection],
        kind: EditKind,
    ) -> Vec<Change> {
        edits.retain(|(start, end, text)| start != end || !text.is_empty());
        if edits.is_empty() {
            return Vec::new();
        }
        // Back to front, so earlier ranges stay valid as later ones change length.
        edits.sort_by_key(|e| std::cmp::Reverse(e.0));
        let mut changes = Vec::new();
        let mut recorded = Vec::new();
        for (start, end, text) in edits {
            let removed: String = self.text.slice(start..end).into();
            changes.push(self.splice(start, end - start, &text));
            recorded.push(Edit { at: start, removed, inserted: text });
        }
        self.redo.clear();

        let now = Instant::now();
        let saved_id = self.saved_id;
        if let Some(last) = self.undo.last_mut() {
            // Never merge into the saved step, or undo would skip past the saved state.
            let mergeable = kind != EditKind::Other
                && last.kind == kind
                && last.id != saved_id
                && now.duration_since(last.time) < COALESCE_WINDOW
                && !starts_new_word(last, &recorded, kind);
            if mergeable {
                last.edits.extend(recorded);
                last.after = after.to_vec();
                last.time = now;
                return changes;
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        let (before, after) = (before.to_vec(), after.to_vec());
        self.undo.push(Transaction { id, kind, edits: recorded, before, after, time: now });
        changes
    }

    /// Ends the current undo step, so the next edit starts a new one (the caret moved).
    pub fn seal(&mut self) {
        if let Some(last) = self.undo.last_mut() {
            last.kind = EditKind::Other;
        }
    }

    pub fn undo(&mut self) -> Option<(Vec<Selection>, Vec<Change>)> {
        let tx = self.undo.pop()?;
        let mut changes = Vec::new();
        for edit in tx.edits.iter().rev() {
            let len = edit.inserted.chars().count();
            changes.push(self.splice(edit.at, len, &edit.removed));
        }
        let selections = tx.before.clone();
        self.redo.push(tx);
        // Typing after an undo starts a fresh step.
        self.seal();
        Some((selections, changes))
    }

    pub fn redo(&mut self) -> Option<(Vec<Selection>, Vec<Change>)> {
        let mut tx = self.redo.pop()?;
        let mut changes = Vec::new();
        for edit in &tx.edits {
            let len = edit.removed.chars().count();
            changes.push(self.splice(edit.at, len, &edit.inserted));
        }
        let selections = tx.after.clone();
        // A redone step must never merge with the next keystroke.
        tx.kind = EditKind::Other;
        self.undo.push(tx);
        Some((selections, changes))
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

/// Typing a space after a word starts a new undo step, so undo works word by word.
fn starts_new_word(last: &Transaction, next: &[Edit], kind: EditKind) -> bool {
    let typed_space = next.first().and_then(|e| e.inserted.chars().next()).is_some_and(char::is_whitespace);
    let after_word =
        last.edits.last().and_then(|e| e.inserted.chars().last()).is_some_and(|c| !c.is_whitespace());
    kind == EditKind::Insert && typed_space && after_word
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
            let (before, after) = ([Selection::caret(pos)], [Selection::caret(pos + 1)]);
            doc.edit(vec![(pos, pos, c.to_string())], &before, &after, EditKind::Insert);
        }
    }

    /// A temp folder that is deleted when the test ends.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    impl std::ops::Deref for Scratch {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }

    fn scratch_dir(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("oxide-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    #[test]
    fn typing_is_undone_as_one_step() {
        let mut doc = Document::empty(None);
        type_chars(&mut doc, 0, "hello");
        assert_eq!(doc.text.to_string(), "hello");
        let (sels, _) = doc.undo().unwrap();
        assert_eq!(doc.text.to_string(), "");
        assert_eq!(sels, vec![Selection::caret(0)]);
        doc.redo().unwrap();
        assert_eq!(doc.text.to_string(), "hello");
    }

    #[test]
    fn undo_goes_word_by_word_and_stops_where_the_caret_moved() {
        let mut doc = Document::empty(None);
        type_chars(&mut doc, 0, "one two");
        doc.undo();
        assert_eq!(doc.text.to_string(), "one");
        doc.redo();
        doc.seal();
        type_chars(&mut doc, 7, "!");
        doc.undo();
        assert_eq!(doc.text.to_string(), "one two");
    }

    #[test]
    fn typing_a_newline_starts_a_new_step() {
        let mut doc = Document::empty(None);
        type_chars(&mut doc, 0, "ab");
        doc.edit(vec![(2, 2, "\n".into())], &[Selection::caret(2)], &[Selection::caret(3)], EditKind::Other);
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
        doc.edit(vec![(0, 3, "ONE".into())], &[Selection::caret(0)], &[Selection::caret(3)], EditKind::Other);
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
    fn restored_unsaved_text_stays_dirty_until_saved() {
        let dir = scratch_dir("restore");
        let path = dir.join("r.txt");
        let mut doc = Document::with_unsaved(Some(path.clone()), "kept\r\n".into());
        assert!(doc.is_dirty());
        assert_eq!(doc.line_ending, "\r\n");
        doc.save().unwrap();
        assert!(!doc.is_dirty());
        assert_eq!(fs::read_to_string(path).unwrap(), "kept\r\n");
    }

    #[test]
    fn replace_many_is_one_undo_step() {
        let mut doc = Document::empty(None);
        doc.text = Rope::from_str("a-b-c");
        let edits = vec![(1, 2, "+".into()), (3, 4, "++".into())];
        doc.edit(edits, &[Selection::caret(0)], &[Selection::caret(0)], EditKind::Other);
        assert_eq!(doc.text.to_string(), "a+b++c");
        doc.undo();
        assert_eq!(doc.text.to_string(), "a-b-c");
        doc.redo();
        assert_eq!(doc.text.to_string(), "a+b++c");
    }

    #[test]
    fn changes_report_replaced_line_ranges() {
        let mut doc = Document::empty(None);
        doc.text = Rope::from_str("a\nb\nc\n");
        // Join lines 1 and 2 ("b\nc" -> "bc").
        let change = doc.edit(
            vec![(3, 4, "".into())],
            &[Selection::caret(3)],
            &[Selection::caret(3)],
            EditKind::Other,
        )[0];
        assert_eq!((change.first_line, change.old_lines, change.new_lines), (1, 2, 1));
        let change = doc.edit(
            vec![(0, 0, "x\ny\n".into())],
            &[Selection::caret(0)],
            &[Selection::caret(4)],
            EditKind::Other,
        )[0];
        assert_eq!((change.first_line, change.old_lines, change.new_lines), (0, 1, 3));
    }
}
