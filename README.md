# oxide

oxide is a super light text editor built in Rust.
Not an IDE: it opens instantly, edits text well, and leaves the rest to add-ons.

## Install

**macOS:** build the app, install it in `/Applications`, and put the `ox` command on your PATH:

```sh
scripts/install-macos.sh
```

Then open files from a terminal, Finder ("Open With", or drop on the Dock icon), or Launchpad:

```sh
ox file.rs other.md          # opens each file in a tab
ox --wait COMMIT_EDITMSG     # blocks until those tabs close: git config core.editor "ox --wait"
```

Only one copy of Oxide runs: `ox` hands its files to it over a local socket and returns in
a few milliseconds. Quitting keeps your tabs and any unsaved text (hot exit) and brings them
back next time; the session is also saved a couple of seconds after each change, so a crash
loses little.

`APP_DIR` and `BIN_DIR` change where it installs (default `/Applications` and `~/.local/bin`).
Run the script again to update.

**Linux:** `scripts/install-linux.sh` builds oxide and installs `oxide` and `ox` into
`~/.local/bin`, with a desktop entry and icon. Release tarballs include the same script.

**Windows:** unzip a release and put the folder on your `PATH`; `ox.cmd` and `oxide.exe`
work the same way as on macOS. Or `cargo build --release`.

Prebuilt packages for all three come from the [Release workflow](.github/workflows/release.yml),
and every push is built and tested on macOS, Windows, and Linux.

## Keys

`Cmd` on macOS, `Ctrl` elsewhere.

| Keys | Action |
| --- | --- |
| `Cmd+S` / `Cmd+Shift+S` | Save / save as (atomic; follows symlinks; keeps line endings) |
| `Cmd+N` / `Cmd+O` | New tab / open files (or drop files on the window) |
| `Cmd+W` | Close tab (press twice to discard unsaved changes) |
| `Cmd+Q` | Quit, keeping unsaved work for next time |
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
| `Shift+Alt+F` | Format the file with its formatter add-on |
| `Cmd+,` | Open settings |

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
- **`instance.rs`** and **`session.rs`**: the single running instance and hot exit.
- **`window.rs`**: tabs, the find and go-to bar, input, scrolling, and painting a frame. Nothing redraws unless something
  changed, so an idle window uses no CPU.

## Settings and formatters

`Cmd+,` opens `~/.config/oxide/settings.toml` (`%APPDATA%\oxide\settings.toml` on Windows),
created with every option commented. Saving it applies the changes immediately.

```toml
font_family = "JetBrains Mono"
font_size = 14
theme = "base16-ocean.dark"     # built in, or NAME.tmTheme in ~/.config/oxide/themes/
tab_size = 4
translate_tabs_to_spaces = true
detect_indentation = true
format_on_save = true

# A formatter reads the file on stdin and prints the result; $FILE is the file's path.
[[formatter]]
extensions = ["rs"]
command = ["rustfmt", "--edition", "2021", "--emit", "stdout"]

[[formatter]]
extensions = ["js", "ts", "json", "css", "md"]
command = ["prettier", "--stdin-filepath", "$FILE"]
```

Formatting runs in the background, applies only the lines that changed as one undo step,
and keeps your caret where it was. If a formatter fails, the file still saves.

## Add-ons

Drop syntax definitions (`.sublime-syntax` YAML grammars) into `~/.config/oxide/syntaxes/`
(`%APPDATA%\oxide\syntaxes\` on Windows) to add languages.

Debugging: `OXIDE_TRACE=1` prints startup timings; `OXIDE_SNAPSHOT=frame.bmp` writes every
frame to an image.

## Roadmap

1. ~~**Daily-driver basics**: find and replace, go to line, tabs, open and save-as
   dialogs, input methods (IME).~~ Done.
2. ~~**Instant re-open**: one running instance; `ox file` hands the path over a socket
   and opens a tab in milliseconds. Hot exit, so unsaved buffers survive quitting.~~ Done.
3. **Feel**: multiple cursors, auto-closing brackets, comment toggling, a minimap,
   colour schemes, settings file.
4. **Add-ons**: ~~declarative formatters, themes, and syntaxes~~ (done); keymaps; then
   sandboxed WASM add-ons for anything needing code. A public registry later.
5. **LSP as an add-on**, not core. The core will expose popups, underlines, and
   UTF-16 positions so it can be added without a rewrite.

Startup: after the first launch, oxide loads only the font files it used last time
(`fonts.json` in its data folder) and scans the rest in the background, so a cold start
doesn't wait on the whole font library.

Known limits today: one selection; caret moves by code point rather than grapheme
cluster; lines over 16 KB aren't highlighted; no word wrap.

## License

MIT. See [LICENSE](LICENSE).
