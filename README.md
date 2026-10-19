# oxide

oxide is a super light text editor built in Rust.
Not an IDE: it opens instantly, edits text well, and leaves the rest to add-ons.

```sh
cargo build --release
./target/release/oxide path/to/file      # returns to the shell immediately
./target/release/oxide --wait file       # blocks until the window closes (for `git commit`)
```

## Keys

`Cmd` on macOS, `Ctrl` elsewhere.

| Keys | Action |
| --- | --- |
| `Cmd+S` | Save (atomic; follows symlinks; keeps line endings) |
| `Cmd+Z` / `Cmd+Shift+Z` | Undo / redo (typing is grouped into word-sized steps) |
| `Cmd+C` / `Cmd+X` / `Cmd+V` | Copy / cut / paste (copy or cut with no selection takes the whole line) |
| `Cmd+A` | Select all |
| `Tab` / `Shift+Tab` | Indent / outdent (whole lines when the selection spans several) |
| `Alt+←/→` (`Ctrl` elsewhere) | Move by word; add `Shift` to select |
| `Cmd+←/→`, `Home`/`End` | Line start (smart: indentation first) / end |
| `Cmd+=` / `Cmd+-` / `Cmd+0` | Zoom in / out / reset |
| `Cmd+W`, `Cmd+Q` | Close / quit (press twice to discard unsaved changes) |

Double-click selects a word, triple-click a line.

## How it works

Two rules keep it fast: own the whole rendering pipeline, and keep anything slow off the
thread that handles typing.

- **`document.rs`**: the text as a rope (`ropey`), so edits are O(log n) on any size of file,
  with undo history and saved/dirty tracking.
- **`editor.rs`**: editing commands that only need the text (typing, deleting, word moves).
- **`highlight.rs`**: a background thread per document runs regex-based syntax grammars
  (`syntect`). It stores parser state at each line start, so after an edit it
  re-parses only until the state converges with the previous pass, usually a line or two.
  The UI draws plain text until colours arrive and never waits for them.
- **`render.rs`**: CPU rendering, which starts faster than a GPU pipeline. `cosmic-text` shapes lines
  (with font fallback), `swash` rasterises glyphs, and pixels are blended straight into the
  window buffer (`softbuffer`). Shaped lines are cached between frames.
- **`window.rs`**: input, scrolling, and painting a frame. Nothing redraws unless something
  changed, so an idle window uses no CPU.

Add-ons: drop syntax definitions (`.sublime-syntax` YAML grammars) into `~/.config/oxide/syntaxes/`
(`%APPDATA%\oxide\syntaxes\` on Windows) to add languages.

Debugging: `OXIDE_TRACE=1` prints startup timings; `OXIDE_SNAPSHOT=frame.bmp` writes every
frame to an image.

## Roadmap

1. **Daily-driver basics**: find and replace, go to line, tabs, a Save As dialog for
   untitled files, input methods (IME) for non-Latin text, a blinking caret.
2. **Instant re-open**: one running instance; `oxide file` hands the path over a socket
   and opens a tab in milliseconds. Hot exit, so unsaved buffers survive quitting.
3. **Feel**: multiple cursors, auto-closing brackets, comment toggling, a minimap,
   colour schemes, settings file.
4. **Add-ons**: declarative add-ons (formatter = "pipe the buffer through `rustfmt`",
   themes, keymaps), then sandboxed WASM add-ons for anything needing code. A public
   registry later.
5. **LSP as an add-on**, not core. The core will expose popups, underlines, and
   UTF-16 positions so it can be added without a rewrite.

Known limits today: one selection; caret moves by code point rather than grapheme
cluster; lines over 16 KB aren't highlighted; no word wrap.
