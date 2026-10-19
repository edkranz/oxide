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
| `Cmd+S` / `Cmd+Shift+S` | Save / save as (atomic; follows symlinks; keeps line endings) |
| `Cmd+N` / `Cmd+O` | New tab / open files (or drop files on the window) |
| `Cmd+W`, `Cmd+Q` | Close tab / quit (press twice to discard unsaved changes) |
| `Ctrl+Tab`, `Cmd+1`…`9` | Next tab / jump to tab (`Cmd+Shift+[`/`]` on macOS, `Ctrl+PageUp`/`PageDown` elsewhere) |
| `Cmd+Z` / `Cmd+Shift+Z` | Undo / redo (typing is grouped into word-sized steps) |
| `Cmd+C` / `Cmd+X` / `Cmd+V` | Copy / cut / paste (copy or cut with no selection takes the whole line) |
| `Cmd+F` | Find: live as you type; `Enter` / `Shift+Enter` for next / previous |
| `Cmd+Alt+F` (`Ctrl+H` elsewhere) | Find and replace: `Tab` switches fields, `Enter` replaces one, `Cmd+Alt+Enter` (`Ctrl+Alt+Enter`) replaces all |
| `Cmd+Alt+C` / `R` / `W` (`Alt+…` elsewhere) | Toggle case-sensitive / regex / whole-word (or click `Aa` `.*` `\b`) |
| `Cmd+G` / `Cmd+Shift+G` (`F3` / `Shift+F3`) | Find next / previous with the find bar closed |
| `Ctrl+G` | Go to line (`line` or `line:column`, previews as you type) |
| `Cmd+A` | Select all |
| `Tab` / `Shift+Tab` | Indent / outdent (whole lines when the selection spans several) |
| `Alt+←/→` (`Ctrl` elsewhere) | Move by word; add `Shift` to select |
| `Cmd+←/→`, `Home`/`End` | Line start (smart: indentation first) / end |
| `Cmd+=` / `Cmd+-` / `Cmd+0` | Zoom in / out / reset |

Double-click selects a word, triple-click a line. Middle-click a tab to close it.
Input methods (Chinese, Japanese, Korean, accented characters) are supported.

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
- **`find.rs`**: find and replace on top of the `regex` crate; literal queries are escaped
  so both modes share one path.
- **`window.rs`**: tabs, the find and go-to bar, input, scrolling, and painting a frame. Nothing redraws unless something
  changed, so an idle window uses no CPU.

Add-ons: drop syntax definitions (`.sublime-syntax` YAML grammars) into `~/.config/oxide/syntaxes/`
(`%APPDATA%\oxide\syntaxes\` on Windows) to add languages.

Debugging: `OXIDE_TRACE=1` prints startup timings; `OXIDE_SNAPSHOT=frame.bmp` writes every
frame to an image.

## Roadmap

1. ~~**Daily-driver basics**: find and replace, go to line, tabs, open and save-as
   dialogs, input methods (IME).~~ Done.
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
