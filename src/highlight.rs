//! Syntax highlighting on a background thread, one per document.
//!
//! The thread keeps the parser state at the start of every line. After an edit it
//! re-parses from the first changed line and stops as soon as the state at a line
//! start matches what it had before, so typing re-highlights only a line or two.
//! Results stream back to the UI in batches; the UI paints uncoloured text until then.

use crate::document::Change;
use ropey::Rope;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter as ThemeHighlighter, Theme,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

/// Lines longer than this are left uncoloured; regex grammars get slow on them.
const MAX_LINE_BYTES: usize = 16 * 1024;
/// How long the thread works before sending a batch to the UI.
const BATCH_TIME: Duration = Duration::from_millis(8);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StyledRun {
    pub len: u32,
    pub color: u32,
    pub bold: bool,
    pub italic: bool,
}

pub type LineStyle = Arc<[StyledRun]>;

pub struct Batch {
    pub version: u64,
    pub start_line: usize,
    pub lines: Vec<LineStyle>,
    pub syntax_name: Option<String>,
}

enum Request {
    Reset { version: u64, text: Rope, path: Option<PathBuf> },
    Edit { version: u64, text: Rope, changes: Vec<Change> },
}

pub struct Highlighter {
    tx: Sender<Request>,
}

impl Highlighter {
    pub fn spawn(theme: Theme, send: impl Fn(Batch) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("highlight".into())
            .spawn(move || Worker::new(theme, rx, send).run())
            .expect("spawn highlight thread");
        Self { tx }
    }

    pub fn reset(&self, version: u64, text: Rope, path: Option<PathBuf>) {
        let _ = self.tx.send(Request::Reset { version, text, path });
    }

    /// `changes` must be in the order they were applied to produce `text`.
    pub fn edit(&self, version: u64, text: Rope, changes: Vec<Change>) {
        let _ = self.tx.send(Request::Edit { version, text, changes });
    }
}

/// The UI's copy of line colours, kept aligned with the text while the highlighter
/// catches up.
pub struct StyleCache {
    /// Colours per line; `None` until the highlighter reaches it.
    lines: Vec<Option<LineStyle>>,
    /// Edits the highlighter has not reported back on yet, with the version each produced.
    pending: VecDeque<(u64, Change)>,
}

impl StyleCache {
    pub fn new(line_count: usize) -> Self {
        Self { lines: vec![None; line_count], pending: VecDeque::new() }
    }

    pub fn get(&self, line: usize) -> Option<LineStyle> {
        self.lines.get(line).cloned().flatten()
    }

    /// Records `changes`, the last of which produced document `version`.
    pub fn record(&mut self, version: u64, changes: &[Change]) {
        let first_version = version + 1 - changes.len() as u64;
        for (i, change) in changes.iter().enumerate() {
            let first = change.first_line.min(self.lines.len());
            let end = (change.first_line + change.old_lines).min(self.lines.len());
            // Keep the edited line's old colours until fresh ones arrive, to avoid a flash.
            let keep = self.get(first);
            let fresh = std::iter::once(keep).chain(std::iter::repeat_n(None, change.new_lines - 1));
            self.lines.splice(first..end, fresh);
            self.pending.push_back((first_version + i as u64, *change));
        }
    }

    pub fn apply(&mut self, batch: Batch) {
        while self.pending.front().is_some_and(|(v, _)| *v <= batch.version) {
            self.pending.pop_front();
        }
        // Map line numbers through edits made after the highlighter's snapshot. Lines
        // inside an edited region are stale; the highlighter is already redoing them.
        let mut lines: Vec<(usize, LineStyle)> =
            batch.lines.into_iter().enumerate().map(|(i, s)| (batch.start_line + i, s)).collect();
        for (_, change) in &self.pending {
            lines.retain_mut(|(line, _)| {
                if *line < change.first_line {
                    true
                } else if *line < change.first_line + change.old_lines {
                    false
                } else {
                    *line = *line + change.new_lines - change.old_lines;
                    true
                }
            });
        }
        for (line, style) in lines {
            if let Some(slot) = self.lines.get_mut(line) {
                *slot = Some(style);
            }
        }
    }
}

/// Built-in grammars, plus any `.sublime-syntax` files in the user's syntaxes folder.
pub fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(|| {
        let defaults = SyntaxSet::load_defaults_newlines();
        let Some(dir) = crate::config_dir().map(|d| d.join("syntaxes")).filter(|d| d.is_dir()) else {
            return defaults;
        };
        let mut builder = defaults.into_builder();
        if let Err(e) = builder.add_from_folder(&dir, true) {
            eprintln!("oxide: skipping syntaxes in {}: {e}", dir.display());
        }
        builder.build()
    })
}

struct Worker<F> {
    theme: Theme,
    rx: Receiver<Request>,
    send: F,
    syntax: Option<&'static SyntaxReference>,
    text: Rope,
    version: u64,
    /// Parser state at the start of each line; `None` where it must be recomputed.
    states: Vec<Option<(ParseState, HighlightState)>>,
    /// First line still to highlight; `None` once the document is fully highlighted.
    dirty_from: Option<usize>,
    /// Line the parser must reach before it may stop at a matching state.
    must_reach: usize,
    announce_syntax: bool,
}

impl<F: Fn(Batch)> Worker<F> {
    fn new(theme: Theme, rx: Receiver<Request>, send: F) -> Self {
        Self {
            theme,
            rx,
            send,
            syntax: None,
            text: Rope::new(),
            version: 0,
            states: Vec::new(),
            dirty_from: None,
            must_reach: 0,
            announce_syntax: false,
        }
    }

    fn run(mut self) {
        loop {
            let request = if self.dirty_from.is_none() {
                match self.rx.recv() {
                    Ok(r) => Some(r),
                    Err(_) => return,
                }
            } else {
                match self.rx.try_recv() {
                    Ok(r) => Some(r),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                }
            };
            match request {
                Some(request) => self.apply(request),
                None => self.work(),
            }
        }
    }

    fn apply(&mut self, request: Request) {
        match request {
            Request::Reset { version, text, path } => {
                let syntax = pick_syntax(&text, path.as_deref());
                self.states = vec![None; text.len_lines()];
                self.states[0] = Some(self.initial_state(syntax));
                self.syntax = Some(syntax);
                self.text = text;
                self.version = version;
                self.dirty_from = Some(0);
                self.must_reach = 0;
                self.announce_syntax = true;
            }
            Request::Edit { version, text, changes } => {
                if self.syntax.is_none() {
                    return;
                }
                for change in changes {
                    self.splice(change);
                }
                self.text = text;
                self.version = version;
                if self.states.len() != self.text.len_lines() {
                    // Should not happen; recover by re-highlighting everything.
                    let syntax = self.syntax.unwrap();
                    self.states = vec![None; self.text.len_lines()];
                    self.states[0] = Some(self.initial_state(syntax));
                    self.dirty_from = Some(0);
                }
            }
        }
    }

    fn initial_state(&self, syntax: &SyntaxReference) -> (ParseState, HighlightState) {
        let highlighter = ThemeHighlighter::new(&self.theme);
        (ParseState::new(syntax), HighlightState::new(&highlighter, ScopeStack::new()))
    }

    fn splice(&mut self, Change { first_line, old_lines, new_lines }: Change) {
        // Keep the state at `first_line` (it depends only on earlier lines) and the
        // one just past the edit, which is what convergence is checked against.
        let end = (first_line + old_lines).min(self.states.len());
        self.states.splice(first_line + 1..end, std::iter::repeat_n(None, new_lines - 1));
        if self.must_reach > first_line {
            self.must_reach = (self.must_reach + new_lines).saturating_sub(old_lines);
        }
        self.must_reach = self.must_reach.max(first_line + new_lines);
        self.dirty_from = Some(self.dirty_from.map_or(first_line, |d| d.min(first_line)));
    }

    fn work(&mut self) {
        let Some(start) = self.dirty_from else {
            return;
        };
        let syntax_set = syntax_set();
        let highlighter = ThemeHighlighter::new(&self.theme);
        let (mut parse, mut hl) = self.states[start].clone().expect("state before dirty line");
        let deadline = Instant::now() + BATCH_TIME;
        let line_count = self.text.len_lines();
        let mut lines = Vec::new();
        let mut line = start;
        let mut buf = String::new();
        self.dirty_from = None;

        while line < line_count {
            buf.clear();
            for chunk in self.text.line(line).chunks() {
                buf.push_str(chunk);
            }
            let content_len = buf.trim_end_matches(['\n', '\r']).len();
            buf.truncate(content_len);
            buf.push('\n');

            let runs = if content_len > MAX_LINE_BYTES {
                None
            } else {
                parse.parse_line(&buf, syntax_set).ok().map(|ops| {
                    let mut runs: Vec<StyledRun> = Vec::new();
                    let mut offset = 0;
                    for (style, piece) in HighlightIterator::new(&mut hl, &ops, &buf, &highlighter) {
                        let len = piece.len().min(content_len.saturating_sub(offset));
                        offset += piece.len();
                        if len == 0 {
                            continue;
                        }
                        let c = style.foreground;
                        let run = StyledRun {
                            len: len as u32,
                            color: (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32,
                            bold: style.font_style.contains(FontStyle::BOLD),
                            italic: style.font_style.contains(FontStyle::ITALIC),
                        };
                        match runs.last_mut() {
                            Some(last)
                                if (last.color, last.bold, last.italic)
                                    == (run.color, run.bold, run.italic) =>
                            {
                                last.len += run.len
                            }
                            _ => runs.push(run),
                        }
                    }
                    runs
                })
            };
            lines.push(runs.unwrap_or_default().into());
            line += 1;

            if line < line_count {
                let next = Some((parse.clone(), hl.clone()));
                if line >= self.must_reach && self.states[line] == next {
                    break;
                }
                self.states[line] = next;
                if Instant::now() >= deadline {
                    self.dirty_from = Some(line);
                    break;
                }
            }
        }

        (self.send)(Batch {
            version: self.version,
            start_line: start,
            lines,
            syntax_name: self.announce_syntax.then(|| self.syntax.unwrap().name.clone()),
        });
        self.announce_syntax = false;
    }
}

fn pick_syntax(text: &Rope, path: Option<&Path>) -> &'static SyntaxReference {
    let set = syntax_set();
    let by_name = path.and_then(|p| {
        let name = p.file_name()?.to_str()?;
        let ext = p.extension().and_then(|e| e.to_str());
        set.find_syntax_by_extension(name).or_else(|| set.find_syntax_by_extension(ext?))
    });
    by_name
        .or_else(|| {
            let first: String = text.line(0).chars().take(256).collect();
            set.find_syntax_by_first_line(&first)
        })
        .unwrap_or_else(|| set.find_syntax_plain_text())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, EditKind, Selection};
    use syntect::highlighting::ThemeSet;

    const SAMPLE: &str = r#"/// Docs
fn main() {
    let s = "string";
    /* block
       comment */
    println!("{}", s); // trailing
}
"#;

    fn theme() -> Theme {
        ThemeSet::load_defaults().themes.remove("base16-ocean.dark").unwrap()
    }

    /// Applies batches until the highlighter has been quiet for a while.
    fn settle(rx: &Receiver<Batch>, cache: &mut StyleCache) {
        while let Ok(batch) = rx.recv_timeout(Duration::from_millis(500)) {
            cache.apply(batch);
        }
    }

    fn highlight_from_scratch(text: &Rope) -> StyleCache {
        let (tx, rx) = mpsc::channel();
        let highlighter = Highlighter::spawn(theme(), move |b| {
            let _ = tx.send(b);
        });
        highlighter.reset(0, text.clone(), Some("x.rs".into()));
        let mut cache = StyleCache::new(text.len_lines());
        settle(&rx, &mut cache);
        cache
    }

    /// Random edits (including ones that open and close comments and strings, so
    /// highlighting changes far from the edit) streamed while the highlighter is busy
    /// must end up exactly where a fresh highlight of the final text does.
    #[test]
    fn incremental_highlighting_matches_a_fresh_pass() {
        let (tx, rx) = mpsc::channel();
        let highlighter = Highlighter::spawn(theme(), move |b| {
            let _ = tx.send(b);
        });
        let mut doc = Document::empty(Some("x.rs".into()));
        doc.text = Rope::from_str(&SAMPLE.repeat(40));
        highlighter.reset(doc.version, doc.text.clone(), doc.path.clone());
        let mut cache = StyleCache::new(doc.text.len_lines());

        let snippets = ["/*", "*/", "\"", "\n", "fn x() {", "}", "// c\n", "let a = 1;", "\n\n"];
        let mut seed: u64 = 0x9e3779b97f4a7c15;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for step in 0..300usize {
            let len = doc.text.len_chars();
            let at = next(len + 1);
            let changes = if next(3) == 0 {
                let end = (at + next(12)).min(len);
                doc.edit(
                    vec![(at, end, String::new())],
                    &[Selection::caret(at)],
                    &[Selection::caret(at)],
                    EditKind::Other,
                )
            } else {
                let s = snippets[next(snippets.len())];
                doc.edit(
                    vec![(at, at, s.to_string())],
                    &[Selection::caret(at)],
                    &[Selection::caret(at)],
                    EditKind::Other,
                )
            };
            cache.record(doc.version, &changes);
            highlighter.edit(doc.version, doc.text.clone(), changes);
            if step.is_multiple_of(7) {
                while let Ok(batch) = rx.try_recv() {
                    cache.apply(batch);
                }
            }
        }
        settle(&rx, &mut cache);

        let fresh = highlight_from_scratch(&doc.text);
        for line in 0..doc.text.len_lines() {
            assert_eq!(cache.get(line), fresh.get(line), "line {line}: {:?}", doc.text.line(line));
        }
    }
}
