//! One editor window: tabs, the find and go-to bar, input handling, and painting a frame.

use crate::document::{Document, Selection};
use crate::editor::{self, Editor, Indent};
use crate::find::{self, FindOptions, Search, MATCH_LIMIT};
use crate::format;
use crate::highlight::{Batch, Highlighter, StyleCache};
use crate::input::LineInput;
use crate::instance::Waiter;
use crate::render::{blend, x_for_byte, Canvas, Fonts, Laid, TextLayout};
use crate::session::{TabSnapshot, WindowSnapshot};
use crate::settings::{self, Settings};
use crate::UserEvent;
use std::error::Error;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};
use syntect::highlighting::{Color, Theme};
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Window, WindowId};

/// A second close or quit within this window discards unsaved changes.
pub const CONFIRM_WINDOW: Duration = Duration::from_secs(4);
const MULTI_CLICK: Duration = Duration::from_millis(400);
const MESSAGE_TIME: Duration = Duration::from_secs(4);
/// The caret blinks with this half-period, and holds still after this long without input.
const BLINK: Duration = Duration::from_millis(530);
const BLINK_IDLE: Duration = Duration::from_secs(15);
const MAC: bool = cfg!(target_os = "macos");
const PRIMARY: &str = if MAC { "Cmd" } else { "Ctrl" };

/// What the app must do after a window handled an event.
pub enum Action {
    None,
    Close,
    Quit,
    Copy(String),
    Paste,
    NewWindow,
}

/// Something to do, asked for by a shortcut or a menu item.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    NewWindow,
    NewTab,
    Open,
    Save,
    SaveAs,
    CloseTab,
    Quit,
    Settings,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    ToggleComment,
    Format,
    AddNextOccurrence,
    SelectAllOccurrences,
    SplitIntoLines,
    AddCaretAbove,
    AddCaretBelow,
    Find,
    FindReplace,
    FindNext,
    FindPrevious,
    GoToLine,
    ToggleCase,
    ToggleRegex,
    ToggleWholeWord,
    WordWrap,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    NextTab,
    PreviousTab,
    Tab(usize),
}

/// What the menu bar shows as ticked for the focused window.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MenuState {
    pub word_wrap: bool,
    pub find: FindOptions,
}

#[derive(Clone, Copy)]
pub struct Palette {
    background: u32,
    foreground: u32,
    caret: u32,
    selection: u32,
    line_highlight: u32,
    find_highlight: u32,
    gutter: u32,
    gutter_active: u32,
    chrome: u32,
    chrome_text: u32,
    panel: u32,
}

impl Palette {
    pub fn from_theme(theme: &Theme) -> Self {
        let s = &theme.settings;
        let rgb = |c: Color| (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32;
        let background = s.background.map_or(0x2b303b, rgb);
        let foreground = s.foreground.map_or(0xc0c5ce, rgb);
        // Theme colours may be translucent; flatten them onto the background.
        let over = |c: Option<Color>, alpha: u32| {
            c.map_or(blend(background, foreground, alpha), |c| blend(background, rgb(c), c.a as u32))
        };
        Self {
            background,
            foreground,
            caret: s.caret.map_or(foreground, rgb),
            selection: over(s.selection, 60),
            line_highlight: over(s.line_highlight, 14),
            find_highlight: over(s.find_highlight, 140),
            gutter: over(s.gutter_foreground, 90),
            gutter_active: blend(background, foreground, 210),
            chrome: blend(background, 0x000000, 70),
            chrome_text: blend(background, foreground, 190),
            panel: blend(background, foreground, 12),
        }
    }
}

/// What a new tab starts with: a document and, when restored, where the view was.
pub struct TabInit {
    pub doc: Document,
    pub sel: Selection,
    pub scroll: (f32, f32),
}

impl From<Document> for TabInit {
    fn from(doc: Document) -> Self {
        Self { doc, sel: Selection::default(), scroll: (0.0, 0.0) }
    }
}

/// One open document.
struct Tab {
    id: u64,
    editor: Editor,
    styles: StyleCache,
    highlighter: Highlighter,
    syntax_name: String,
    scroll_x: f32,
    /// The view's top: a line, and how far into it (pixels) the view starts.
    top_line: usize,
    top_offset: f32,
    wrap: bool,
    /// Remembered x per caret for up/down, so carets keep their column across short lines.
    goals: Vec<f32>,
    close_armed: Option<Instant>,
    matches: Option<MatchCache>,
}

/// Find results, reused until the text, query, or options change.
struct MatchCache {
    version: u64,
    query: String,
    options: FindOptions,
    result: Result<Rc<Vec<(usize, usize)>>, String>,
}

impl Tab {
    fn name(&self) -> String {
        self.editor
            .doc
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map_or("untitled".into(), |n| n.to_string_lossy().into_owned())
    }

    /// An empty, unsaved, untitled tab that opening a file may replace.
    fn is_pristine(&self) -> bool {
        self.editor.doc.path.is_none() && !self.editor.doc.is_dirty() && self.editor.len() == 0
    }
}

/// The bar above the status bar.
enum Panel {
    Find { query: LineInput, replace: Option<LineInput>, in_replace: bool, anchor: usize },
    GoTo { input: LineInput, origin: Selection, origin_scroll: (f32, usize, f32) },
}

#[derive(Clone, Copy)]
enum Toggle {
    Case,
    Regex,
    Word,
}

/// A clickable area recorded while painting.
#[derive(Clone, Copy)]
enum Hit {
    Tab(usize),
    CloseTab(usize),
    Toggle(Toggle),
    PanelRow(usize),
}

struct Region {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    hit: Hit,
}

pub struct Win {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    layout: TextLayout,
    palette: Palette,
    theme: Theme,
    proxy: EventLoopProxy<UserEvent>,
    tabs: Vec<Tab>,
    active: usize,
    next_tab_id: u64,
    panel: Option<Panel>,
    /// Whether keystrokes go to the panel rather than the document.
    panel_focus: bool,
    find_options: FindOptions,
    last_query: String,
    last_replace: String,
    regions: Vec<Region>,
    /// Text being composed by an input method, drawn at the caret until committed.
    preedit: Option<String>,
    ime_area: Option<(i32, i32)>,
    mouse: (f32, f32),
    dragging: bool,
    last_click: Option<(Instant, (f32, f32), u32)>,
    message: Option<(String, Instant)>,
    close_armed: Option<Instant>,
    font_points: f32,
    /// `ox --wait` clients, each released when all of its tabs have closed.
    waiters: Vec<(Vec<u64>, Waiter)>,
    /// Set when something the session records has changed since it was last taken.
    session_changed: bool,
    settings: Rc<Settings>,
    /// Set when the settings file was saved from this window, so the app reloads it.
    settings_saved: bool,
    focused: bool,
    /// When the caret last became visible; blinking is timed from here.
    blink_start: Instant,
}

impl Win {
    pub fn new(
        event_loop: &ActiveEventLoop,
        fonts: &mut Fonts,
        tabs: Vec<TabInit>,
        active: usize,
        settings: Rc<Settings>,
        (theme, palette): (Theme, Palette),
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<Self, Box<dyn Error>> {
        let attrs = Window::default_attributes()
            .with_title("oxide")
            .with_inner_size(LogicalSize::new(960.0, 680.0))
            .with_visible(false);
        // Lets Linux desktops match the window to oxide.desktop (and its icon).
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
        let attrs = winit::platform::wayland::WindowAttributesExtWayland::with_name(attrs, "oxide", "oxide");
        let window = Rc::new(event_loop.create_window(attrs)?);
        window.set_ime_allowed(true);
        let context = softbuffer::Context::new(window.clone())?;
        let surface = softbuffer::Surface::new(&context, window.clone())?;
        let font_points = settings.font_size;
        let layout =
            TextLayout::new(fonts, font_points * window.scale_factor() as f32, settings.tab_size as u16);

        let mut win = Self {
            window,
            surface,
            layout,
            palette,
            theme,
            proxy,
            tabs: Vec::new(),
            active: 0,
            next_tab_id: 0,
            panel: None,
            panel_focus: false,
            find_options: FindOptions::default(),
            last_query: String::new(),
            last_replace: String::new(),
            regions: Vec::new(),
            preedit: None,
            ime_area: None,
            mouse: (0.0, 0.0),
            dragging: false,
            last_click: None,
            message: None,
            close_armed: None,
            font_points,
            waiters: Vec::new(),
            session_changed: false,
            settings,
            settings_saved: false,
            focused: true,
            blink_start: Instant::now(),
        };
        for tab in tabs {
            win.add_tab(tab);
        }
        if win.tabs.is_empty() {
            win.add_tab(Document::empty(None).into());
        }
        win.active = active.min(win.tabs.len() - 1);
        win.update_title();
        // Paint before showing, so the window never flashes an empty frame.
        win.draw(fonts);
        win.window.set_visible(true);
        // Launched from a terminal, the window must take focus so typing goes to it.
        win.window.focus_window();
        crate::trace("first frame");
        Ok(win)
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    fn ed(&mut self) -> &mut Editor {
        &mut self.tabs[self.active].editor
    }

    fn add_tab(&mut self, TabInit { doc, sel, scroll }: TabInit) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let (proxy, window_id) = (self.proxy.clone(), self.window.id());
        let highlighter = Highlighter::spawn(self.theme.clone(), move |batch| {
            let _ = proxy.send_event(UserEvent::Highlighted(window_id, id, batch));
        });
        highlighter.reset(doc.version, doc.text.clone(), doc.path.clone());
        if doc.read_only {
            self.notify("Not valid UTF-8: opened read-only");
        }
        let default_indent = match self.settings.translate_tabs_to_spaces {
            true => Indent::Spaces(self.settings.tab_size.max(1)),
            false => Indent::Tabs,
        };
        let mut editor = Editor::new(doc, default_indent, self.settings.detect_indentation);
        editor.set_selection(sel);
        self.tabs.push(Tab {
            id,
            styles: StyleCache::new(editor.doc.text.len_lines()),
            editor,
            highlighter,
            syntax_name: String::new(),
            scroll_x: scroll.0,
            // The session stores the top as a fractional line, independent of font size.
            top_line: scroll.1.max(0.0) as usize,
            top_offset: scroll.1.max(0.0).fract() * self.line_height(),
            wrap: self.settings.word_wrap,
            goals: Vec::new(),
            close_armed: None,
            matches: None,
        });
        self.active = self.tabs.len() - 1;
        self.session_changed = true;
        id
    }

    /// Opens each path in a tab, or switches to it if it is already open. Returns the
    /// ids of those tabs.
    pub fn open_paths(&mut self, paths: Vec<PathBuf>) -> Vec<u64> {
        let mut ids = Vec::new();
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        for path in paths {
            let key = canonical(&path);
            let existing = self
                .tabs
                .iter()
                .position(|t| t.editor.doc.path.as_deref().is_some_and(|p| canonical(p) == key));
            if let Some(i) = existing {
                self.active = i;
                ids.push(self.tabs[i].id);
                continue;
            }
            match Document::open(path.clone()) {
                Ok(doc) => {
                    let replace = self.tab().is_pristine().then_some(self.active);
                    ids.push(self.add_tab(doc.into()));
                    if let Some(i) = replace {
                        self.tabs.remove(i);
                        self.active = self.tabs.len() - 1;
                    }
                }
                Err(e) => self.notify(&format!("Could not open {}: {e}", path.display())),
            }
        }
        self.update_title();
        self.window.request_redraw();
        ids
    }

    pub fn tab_ids(&self) -> Vec<u64> {
        self.tabs.iter().map(|t| t.id).collect()
    }

    /// Releases `waiter` once every tab in `ids` has closed.
    pub fn wait_for(&mut self, ids: Vec<u64>, waiter: Waiter) {
        if ids.is_empty() {
            waiter.done();
        } else {
            self.waiters.push((ids, waiter));
        }
    }

    /// Brings the window to the front, restoring it if minimised.
    pub fn present(&self) {
        self.window.set_minimized(false);
        self.window.focus_window();
    }

    /// Whether the session needs saving; clears the flag.
    pub fn take_session_changed(&mut self) -> bool {
        std::mem::take(&mut self.session_changed)
    }

    /// The window's tabs for the session. Untouched empty tabs are left out.
    pub fn snapshot(&self) -> WindowSnapshot {
        let mut tabs = Vec::new();
        let mut active = 0;
        for (i, tab) in self.tabs.iter().enumerate() {
            if tab.is_pristine() {
                continue;
            }
            if i == self.active {
                active = tabs.len();
            }
            let doc = &tab.editor.doc;
            let unsaved = (doc.is_dirty() || doc.path.is_none()).then(|| doc.text.clone());
            let sel = tab.editor.primary();
            tabs.push(TabSnapshot {
                path: doc.path.clone(),
                unsaved,
                anchor: sel.anchor,
                head: sel.head,
                scroll: (tab.scroll_x, tab.top_line as f32 + tab.top_offset / self.line_height()),
            });
        }
        WindowSnapshot { tabs, active }
    }

    pub fn is_dirty(&self) -> bool {
        self.tabs.iter().any(|t| t.editor.doc.is_dirty())
    }

    pub fn menu_state(&self) -> MenuState {
        MenuState { word_wrap: self.tab().wrap, find: self.find_options }
    }

    pub fn notify(&mut self, message: &str) {
        self.message = Some((message.to_string(), Instant::now()));
        self.window.request_redraw();
    }

    /// True if the window may close now; otherwise warns about unsaved changes.
    pub fn confirm_close(&mut self) -> bool {
        if !self.is_dirty() || self.close_armed.is_some_and(|t| t.elapsed() < CONFIRM_WINDOW) {
            return true;
        }
        self.close_armed = Some(Instant::now());
        if let Some(i) = self.tabs.iter().position(|t| t.editor.doc.is_dirty()) {
            self.switch_tab(i);
        }
        self.notify(&format!("Unsaved changes. Close again to discard, or {PRIMARY}+S to save."));
        false
    }

    fn close_tab(&mut self, i: usize) -> Action {
        let tab = &mut self.tabs[i];
        if tab.editor.doc.is_dirty() && tab.close_armed.is_none_or(|t| t.elapsed() >= CONFIRM_WINDOW) {
            tab.close_armed = Some(Instant::now());
            self.switch_tab(i);
            self.notify(&format!("Unsaved changes. Close again to discard, or {PRIMARY}+S to save."));
            return Action::None;
        }
        let closed = self.tabs.remove(i).id;
        self.session_changed = true;
        for (ids, _) in &mut self.waiters {
            ids.retain(|id| *id != closed);
        }
        for (_, waiter) in self.waiters.extract_if(.., |(ids, _)| ids.is_empty()) {
            waiter.done();
        }
        if self.tabs.is_empty() {
            return Action::Close;
        }
        if self.active > i || self.active == self.tabs.len() {
            self.active -= 1;
        }
        self.update_title();
        self.window.request_redraw();
        Action::None
    }

    fn switch_tab(&mut self, i: usize) {
        if i < self.tabs.len() {
            self.active = i;
            self.update_title();
            self.window.request_redraw();
        }
    }

    fn update_title(&self) {
        let tab = self.tab();
        let dirty = tab.editor.doc.is_dirty();
        self.window.set_title(&format!("{}{}", tab.name(), if dirty { " •" } else { "" }));
        #[cfg(target_os = "macos")]
        {
            use winit::platform::macos::WindowExtMacOS;
            self.window.set_document_edited(dirty);
        }
    }

    // Geometry, in physical pixels.

    fn line_height(&self) -> f32 {
        self.layout.metrics.line_height
    }

    fn char_width(&self) -> f32 {
        self.layout.metrics.char_width
    }

    fn tab_bar_height(&self) -> f32 {
        (self.line_height() * 1.5).round()
    }

    fn status_height(&self) -> f32 {
        (self.line_height() * 1.25).round()
    }

    fn panel_row_height(&self) -> f32 {
        (self.line_height() * 1.5).round()
    }

    fn panel_rows(&self) -> usize {
        match &self.panel {
            None => 0,
            Some(Panel::Find { replace: Some(_), .. }) => 2,
            Some(_) => 1,
        }
    }

    fn text_top(&self) -> f32 {
        self.tab_bar_height()
    }

    fn text_bottom(&self) -> f32 {
        self.window.inner_size().height as f32
            - self.status_height()
            - self.panel_rows() as f32 * self.panel_row_height()
    }

    fn text_height(&self) -> f32 {
        self.text_bottom() - self.text_top()
    }

    fn gutter_width(&self) -> f32 {
        let digits = self.tab().editor.doc.text.len_lines().to_string().len().max(3);
        (digits as f32 + 3.0) * self.char_width()
    }

    fn text_left(&self) -> f32 {
        self.gutter_width() + self.char_width()
    }

    /// The line's text and its layout: one row, or several when wrapping.
    fn layout_rows(&mut self, fonts: &mut Fonts, line: usize) -> (String, Laid) {
        let width = self.wrap_width();
        let tab = &self.tabs[self.active];
        let text = tab.editor.line_text(line);
        let runs = tab.styles.get(line);
        let laid =
            self.layout.wrapped(fonts, &text, runs.as_deref().unwrap_or(&[]), self.palette.foreground, width);
        (text, laid)
    }

    /// The wrapping width in pixels for the active tab, if it wraps.
    fn wrap_width(&self) -> Option<f32> {
        let available = self.window.inner_size().width as f32 - self.text_left() - self.char_width() * 2.0;
        self.tab().wrap.then(|| available.max(self.char_width() * 10.0))
    }

    /// How many rows `line` takes on screen.
    fn row_count(&mut self, fonts: &mut Fonts, line: usize) -> usize {
        if self.tab().wrap {
            self.layout_rows(fonts, line).1.rows().len()
        } else {
            1
        }
    }

    /// The line and row that `pos` is on, and its x within the row.
    fn locate(&mut self, fonts: &mut Fonts, pos: usize) -> (usize, usize, f32) {
        let line = self.tab().editor.doc.text.char_to_line(pos);
        let column = pos - self.tab().editor.doc.text.line_to_char(line);
        let (text, laid) = self.layout_rows(fonts, line);
        let byte = char_to_byte(&text, column);
        let row = laid.row_of(byte);
        (line, row, x_for_byte(&laid.rows()[row], byte))
    }

    /// The position on `row` of `line` closest to `x`.
    fn pos_in_row(&mut self, fonts: &mut Fonts, line: usize, row: usize, x: f32) -> usize {
        let (text, laid) = self.layout_rows(fonts, line);
        let byte = laid.byte_at(row.min(laid.rows().len() - 1), &text, x);
        self.tab().editor.doc.text.line_to_char(line) + text[..byte].chars().count()
    }

    /// The row `delta` screen rows away from `row` of `line`; `None` past either end.
    fn step_rows(
        &mut self,
        fonts: &mut Fonts,
        line: usize,
        row: usize,
        delta: isize,
    ) -> Option<(usize, usize)> {
        let line_count = self.tab().editor.doc.text.len_lines();
        let (mut line, mut row) = (line, row as isize + delta);
        while row < 0 {
            line = line.checked_sub(1)?;
            row += self.row_count(fonts, line) as isize;
        }
        loop {
            let rows = self.row_count(fonts, line) as isize;
            if row < rows {
                return Some((line, row as usize));
            }
            row -= rows;
            line += 1;
            if line >= line_count {
                return None;
            }
        }
    }

    /// Scrolls so the top of `row` of `line` sits `space` pixels below the top of the view.
    fn scroll_to_row(&mut self, fonts: &mut Fonts, line: usize, row: usize, space: f32) {
        let line_height = self.line_height();
        let (mut line, mut above, mut need) = (line, row as f32 * line_height, space.max(0.0));
        let (top_line, top_offset) = loop {
            if above >= need {
                break (line, above - need);
            }
            need -= above;
            if line == 0 {
                break (0, 0.0);
            }
            line -= 1;
            above = self.row_count(fonts, line) as f32 * line_height;
        };
        let tab = self.tab_mut();
        (tab.top_line, tab.top_offset) = (top_line, top_offset);
    }

    /// Char index under the mouse pointer.
    fn hit_text(&mut self, fonts: &mut Fonts) -> usize {
        let (x, y) = self.mouse;
        let line_height = self.line_height();
        let last = self.tab().editor.doc.text.len_lines() - 1;
        let x = x - self.text_left() + self.tab().scroll_x;
        let mut y = y - self.text_top() + self.tab().top_offset;
        let mut line = self.tab().top_line.min(last);
        loop {
            let rows = self.row_count(fonts, line);
            let height = rows as f32 * line_height;
            if y < height || line == last {
                let row = ((y.max(0.0) / line_height) as usize).min(rows - 1);
                return self.pos_in_row(fonts, line, row, x);
            }
            y -= height;
            line += 1;
        }
    }

    fn caret_visible(&self) -> bool {
        if !self.focused {
            return false;
        }
        let elapsed = self.blink_start.elapsed();
        !self.settings.caret_blink
            || elapsed >= BLINK_IDLE
            || (elapsed.as_millis() / BLINK.as_millis()).is_multiple_of(2)
    }

    /// When the caret next turns on or off, if it is blinking.
    pub fn next_blink(&self) -> Option<Instant> {
        let elapsed = self.blink_start.elapsed();
        if !self.focused || !self.settings.caret_blink || elapsed >= BLINK_IDLE {
            return None;
        }
        let phase = (elapsed.as_millis() / BLINK.as_millis()) as u32 + 1;
        Some(self.blink_start + BLINK * phase)
    }

    pub fn redraw(&self) {
        self.window.request_redraw();
    }

    fn region_at(&self, (x, y): (f32, f32)) -> Option<Hit> {
        self.regions.iter().rev().find(|r| x >= r.x0 && x < r.x1 && y >= r.y0 && y < r.y1).map(|r| r.hit)
    }

    pub fn handle(&mut self, fonts: &mut Fonts, event: WindowEvent, mods: ModifiersState) -> Action {
        if matches!(
            event,
            WindowEvent::KeyboardInput { .. } | WindowEvent::MouseInput { .. } | WindowEvent::Ime(_)
        ) {
            self.blink_start = Instant::now();
        }
        match event {
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                self.blink_start = Instant::now();
                self.window.request_redraw();
            }
            WindowEvent::Resized(_) => self.window.request_redraw(),
            WindowEvent::ScaleFactorChanged { .. } => self.rebuild_layout(fonts),
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. }
                if event.state == ElementState::Pressed =>
            {
                return self.key(fonts, &event, mods);
            }
            WindowEvent::Ime(Ime::Preedit(text, _)) => {
                self.preedit = (!text.is_empty()).then_some(text);
                self.window.request_redraw();
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.preedit = None;
                self.type_text(fonts, &text);
            }
            WindowEvent::DroppedFile(path) => {
                self.open_paths(vec![path]);
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse = (position.x as f32, position.y as f32);
                if self.dragging {
                    let pos = self.hit_text(fonts);
                    self.ed().move_to(pos, true);
                    self.tab_mut().goals.clear();
                    self.reveal(fonts);
                    self.window.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => match (state, button) {
                (ElementState::Pressed, MouseButton::Left) => return self.click(fonts, mods),
                (ElementState::Pressed, MouseButton::Middle) => {
                    if let Some(Hit::Tab(i) | Hit::CloseTab(i)) = self.region_at(self.mouse) {
                        return self.close_tab(i);
                    }
                }
                (ElementState::Released, MouseButton::Left) => self.dragging = false,
                _ => {}
            },
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => {
                        (x * 3.0 * self.char_width(), y * 3.0 * self.line_height())
                    }
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
                };
                self.scroll_by(fonts, -dx, -dy);
            }
            _ => {}
        }
        Action::None
    }

    fn key(&mut self, fonts: &mut Fonts, event: &KeyEvent, mods: ModifiersState) -> Action {
        if let Some(action) = self.shortcut(fonts, event, mods) {
            return action;
        }
        if self.panel.is_some() && self.panel_focus && self.panel_key(fonts, event, mods) {
            return Action::None;
        }
        self.editor_key(fonts, event, mods)
    }

    /// Window-level shortcuts. `None` means the key was not a shortcut.
    fn shortcut(&mut self, fonts: &mut Fonts, event: &KeyEvent, mods: ModifiersState) -> Option<Action> {
        if let Some(command) = self.command_for(event, mods) {
            return Some(self.run(fonts, command));
        }
        // Swallow other shortcuts on letters so they never type one. Named keys such as
        // Cmd+Left go on to the editor.
        let primary = if MAC { mods.super_key() } else { mods.control_key() && !mods.alt_key() };
        let character = matches!(event.key_without_modifiers(), Key::Character(_));
        (primary && character).then_some(Action::None)
    }

    /// The command a key press asks for, if it is a shortcut.
    fn command_for(&self, event: &KeyEvent, mods: ModifiersState) -> Option<Command> {
        let (shift, ctrl, alt, cmd) =
            (mods.shift_key(), mods.control_key(), mods.alt_key(), mods.super_key());
        // Ctrl+Alt is AltGr on Windows and Linux layouts, which types characters.
        let primary = if MAC { cmd } else { ctrl && !alt };
        let named = match &event.logical_key {
            Key::Named(n) => Some(*n),
            _ => None,
        };
        // Match letters ignoring Shift and Alt, which change the character produced.
        let ch = match event.key_without_modifiers() {
            Key::Character(c) => c.to_lowercase(),
            _ => String::new(),
        };

        // Add a caret above or below: Ctrl+Shift+Up/Down on macOS, Ctrl+Alt+Up/Down elsewhere.
        let add_caret = if MAC { ctrl && shift && !cmd } else { ctrl && alt };
        match named {
            Some(NamedKey::ArrowUp) if add_caret => return Some(Command::AddCaretAbove),
            Some(NamedKey::ArrowDown) if add_caret => return Some(Command::AddCaretBelow),
            Some(NamedKey::F3) if !MAC && alt => return Some(Command::SelectAllOccurrences),
            Some(NamedKey::F3) => return Some(if shift { Command::FindPrevious } else { Command::FindNext }),
            Some(NamedKey::Tab) if ctrl => {
                return Some(if shift { Command::PreviousTab } else { Command::NextTab })
            }
            Some(NamedKey::PageDown) if ctrl && !MAC => return Some(Command::NextTab),
            Some(NamedKey::PageUp) if ctrl && !MAC => return Some(Command::PreviousTab),
            _ => {}
        }
        if MAC && ctrl && cmd && ch == "g" {
            return Some(Command::SelectAllOccurrences);
        }
        if ctrl && !alt && ch == "g" {
            return Some(Command::GoToLine);
        }
        if !MAC && ctrl && !alt && ch == "h" {
            return Some(Command::FindReplace);
        }
        if shift && alt && !ctrl && !cmd && ch == "f" {
            return Some(Command::Format);
        }
        if alt && !ctrl && !cmd && ch == "z" {
            return Some(Command::WordWrap);
        }
        let toggle_mods = if MAC { primary && alt } else { alt && !ctrl };
        if toggle_mods && matches!(self.panel, Some(Panel::Find { .. })) {
            match ch.as_str() {
                "c" => return Some(Command::ToggleCase),
                "r" => return Some(Command::ToggleRegex),
                "w" => return Some(Command::ToggleWholeWord),
                _ => {}
            }
        }
        if !primary {
            return None;
        }
        Some(match ch.as_str() {
            "s" if shift => Command::SaveAs,
            "s" => Command::Save,
            "z" if shift => Command::Redo,
            "z" => Command::Undo,
            "y" if !MAC => Command::Redo,
            "a" => Command::SelectAll,
            "c" => Command::Copy,
            "x" => Command::Cut,
            "v" => Command::Paste,
            "w" => Command::CloseTab,
            "q" if MAC => Command::Quit,
            "n" if shift => Command::NewWindow,
            "n" => Command::NewTab,
            "o" => Command::Open,
            "f" if MAC && alt => Command::FindReplace,
            "f" => Command::Find,
            "d" => Command::AddNextOccurrence,
            "l" if shift => Command::SplitIntoLines,
            "/" => Command::ToggleComment,
            "," => Command::Settings,
            "g" if MAC && shift => Command::FindPrevious,
            "g" if MAC => Command::FindNext,
            "]" if MAC && shift => Command::NextTab,
            "[" if MAC && shift => Command::PreviousTab,
            "=" | "+" => Command::ZoomIn,
            "-" => Command::ZoomOut,
            "0" => Command::ZoomReset,
            d if d.len() == 1 && ("1"..="9").contains(&d) => Command::Tab(d.parse::<usize>().unwrap() - 1),
            _ => return None,
        })
    }

    /// Carries out `command`, from a shortcut or the menu bar.
    pub fn run(&mut self, fonts: &mut Fonts, command: Command) -> Action {
        self.blink_start = Instant::now();
        let n = self.tabs.len();
        match command {
            Command::NewWindow => return Action::NewWindow,
            Command::NewTab => {
                self.add_tab(Document::empty(None).into());
            }
            Command::Open => self.open_dialog(),
            Command::Save => self.save(),
            Command::SaveAs => self.save_as(),
            Command::CloseTab => return self.close_tab(self.active),
            Command::Quit => return Action::Quit,
            Command::Settings => self.open_settings(),
            Command::Undo => self.ed().undo(),
            Command::Redo => self.ed().redo(),
            Command::Cut => {
                let text = self.ed().cut();
                self.after_command(fonts);
                return Action::Copy(text);
            }
            Command::Copy => return Action::Copy(self.tab().editor.copy()),
            Command::Paste => return Action::Paste,
            Command::SelectAll => match self.focused_input() {
                Some(input) => input.select_all(),
                None => self.ed().select_all(),
            },
            Command::ToggleComment => match editor::comment_style(&self.tab().syntax_name) {
                Some(style) => self.ed().toggle_comment(style),
                None => self.notify("No comment style for this file type"),
            },
            Command::Format => self.format(false),
            Command::AddNextOccurrence => {
                if !self.ed().add_next_occurrence() {
                    self.notify("No more occurrences");
                }
            }
            Command::SelectAllOccurrences => self.ed().select_all_occurrences(),
            Command::SplitIntoLines => self.ed().split_into_lines(),
            Command::AddCaretAbove | Command::AddCaretBelow => {
                // Keeps the goal columns, so the new carets line up.
                self.add_caret_vertically(fonts, command == Command::AddCaretBelow);
                self.after_command(fonts);
                return Action::None;
            }
            Command::Find => self.open_find(false),
            Command::FindReplace => self.open_find(true),
            Command::FindNext => self.find_next(fonts, false),
            Command::FindPrevious => self.find_next(fonts, true),
            Command::GoToLine => self.open_goto(),
            Command::ToggleCase => self.toggle(fonts, Toggle::Case),
            Command::ToggleRegex => self.toggle(fonts, Toggle::Regex),
            Command::ToggleWholeWord => self.toggle(fonts, Toggle::Word),
            Command::WordWrap => {
                let tab = self.tab_mut();
                tab.wrap = !tab.wrap;
                tab.scroll_x = 0.0;
                let wrap = tab.wrap;
                self.notify(if wrap { "Word wrap on" } else { "Word wrap off" });
            }
            Command::ZoomIn => self.zoom(fonts, 1.0),
            Command::ZoomOut => self.zoom(fonts, -1.0),
            Command::ZoomReset => self.zoom(fonts, self.settings.font_size - self.font_points),
            // Switching tabs keeps each tab's scroll position, so return before revealing the caret.
            Command::NextTab => {
                self.switch_tab((self.active + 1) % n);
                return Action::None;
            }
            Command::PreviousTab => {
                self.switch_tab((self.active + n - 1) % n);
                return Action::None;
            }
            Command::Tab(i) => {
                self.switch_tab(i);
                return Action::None;
            }
        }
        self.tab_mut().goals.clear();
        self.after_command(fonts);
        Action::None
    }

    fn editor_key(&mut self, fonts: &mut Fonts, event: &KeyEvent, mods: ModifiersState) -> Action {
        let shift = mods.shift_key();
        let primary = if MAC { mods.super_key() } else { mods.control_key() };
        let word = if MAC { mods.alt_key() } else { mods.control_key() };
        let Key::Named(key) = &event.logical_key else {
            if let Some(text) = typed_text(event, mods) {
                self.type_text(fonts, &text);
            }
            return Action::None;
        };
        let mut vertical = false;
        match key {
            NamedKey::ArrowLeft if MAC && primary => self.ed().move_line_start(shift),
            NamedKey::ArrowLeft if word => self.ed().move_word_left(shift),
            NamedKey::ArrowLeft => self.ed().move_left(shift),
            NamedKey::ArrowRight if MAC && primary => self.ed().move_line_end(shift),
            NamedKey::ArrowRight if word => self.ed().move_word_right(shift),
            NamedKey::ArrowRight => self.ed().move_right(shift),
            NamedKey::ArrowUp if MAC && primary => self.ed().move_to(0, shift),
            NamedKey::ArrowDown if MAC && primary => {
                let end = self.tab().editor.len();
                self.ed().move_to(end, shift)
            }
            NamedKey::ArrowUp | NamedKey::ArrowDown | NamedKey::PageUp | NamedKey::PageDown => {
                let page = ((self.text_height() / self.line_height()) as isize - 1).max(1);
                let lines = match key {
                    NamedKey::ArrowUp => -1,
                    NamedKey::ArrowDown => 1,
                    NamedKey::PageUp => -page,
                    _ => page,
                };
                if lines.abs() > 1 {
                    self.scroll_by(fonts, 0.0, lines as f32 * self.line_height());
                }
                self.move_vertical(fonts, lines, shift);
                vertical = true;
            }
            NamedKey::Home if !MAC && primary => self.ed().move_to(0, shift),
            NamedKey::End if !MAC && primary => {
                let end = self.tab().editor.len();
                self.ed().move_to(end, shift)
            }
            NamedKey::Home => self.ed().move_line_start(shift),
            NamedKey::End => self.ed().move_line_end(shift),
            NamedKey::Backspace if MAC && primary => self.ed().delete_to_line_start(),
            NamedKey::Backspace if word => self.ed().delete_word_left(),
            NamedKey::Backspace => self.ed().backspace(),
            NamedKey::Delete if word => self.ed().delete_word_right(),
            NamedKey::Delete => self.ed().delete_forward(),
            NamedKey::Enter => self.ed().newline(),
            NamedKey::Tab if shift => self.ed().outdent(),
            NamedKey::Tab => self.ed().indent(),
            NamedKey::Escape if self.panel.is_some() => self.close_panel(true),
            NamedKey::Escape if self.tab().editor.sels().len() > 1 => self.ed().collapse_to_primary(),
            NamedKey::Escape => {
                let head = self.tab().editor.primary().head;
                self.ed().set_selection(Selection::caret(head))
            }
            _ => {
                if let Some(text) = typed_text(event, mods) {
                    self.type_text(fonts, &text);
                }
                return Action::None;
            }
        }
        if !vertical {
            self.tab_mut().goals.clear();
        }
        self.after_command(fonts);
        Action::None
    }

    /// Keys for the find or go-to bar. Returns false for keys it leaves to the document.
    fn panel_key(&mut self, fonts: &mut Fonts, event: &KeyEvent, mods: ModifiersState) -> bool {
        let Key::Named(key) = &event.logical_key else {
            if let Some(text) = typed_text(event, mods) {
                self.type_text(fonts, &text);
            }
            return true;
        };
        match key {
            NamedKey::Escape => self.close_panel(true),
            NamedKey::Enter => match &self.panel {
                Some(Panel::Find { in_replace: true, .. }) => {
                    let all = if MAC {
                        mods.super_key() && mods.alt_key()
                    } else {
                        mods.control_key() && mods.alt_key()
                    };
                    if all {
                        self.replace_all(fonts);
                    } else {
                        self.replace_next(fonts);
                    }
                }
                Some(Panel::Find { .. }) => self.find_next(fonts, mods.shift_key()),
                Some(Panel::GoTo { .. }) => self.close_panel(false),
                None => {}
            },
            NamedKey::Tab => {
                if let Some(Panel::Find { replace: Some(_), in_replace, .. }) = &mut self.panel {
                    *in_replace = !*in_replace;
                }
            }
            NamedKey::Backspace
            | NamedKey::Delete
            | NamedKey::ArrowLeft
            | NamedKey::ArrowRight
            | NamedKey::Home
            | NamedKey::End => {
                let Some(input) = self.focused_input() else { return false };
                match key {
                    NamedKey::Backspace => input.backspace(),
                    NamedKey::Delete => input.delete(),
                    NamedKey::ArrowLeft => input.left(),
                    NamedKey::ArrowRight => input.right(),
                    NamedKey::Home => input.home(),
                    _ => input.end(),
                }
                self.panel_changed(fonts);
            }
            NamedKey::Space => {
                if let Some(text) = typed_text(event, mods) {
                    self.type_text(fonts, &text);
                }
            }
            _ => return false,
        }
        self.window.request_redraw();
        true
    }

    /// Text from the keyboard or an input method, routed to the panel or the document.
    fn type_text(&mut self, fonts: &mut Fonts, text: &str) {
        if let Some(input) = self.focused_input() {
            input.insert(text);
            self.panel_changed(fonts);
            self.window.request_redraw();
            return;
        }
        self.ed().insert_text(text);
        self.tab_mut().goals.clear();
        self.after_command(fonts);
    }

    pub fn paste(&mut self, fonts: &mut Fonts, text: &str) {
        if let Some(input) = self.focused_input() {
            input.insert(text);
            self.panel_changed(fonts);
            self.window.request_redraw();
            return;
        }
        self.ed().paste(text);
        self.tab_mut().goals.clear();
        self.after_command(fonts);
    }

    fn focused_input(&mut self) -> Option<&mut LineInput> {
        if !self.panel_focus {
            return None;
        }
        match self.panel.as_mut()? {
            Panel::Find { replace: Some(replace), in_replace: true, .. } => Some(replace),
            Panel::Find { query, .. } => Some(query),
            Panel::GoTo { input, .. } => Some(input),
        }
    }

    fn open_find(&mut self, with_replace: bool) {
        let ed = &self.tab().editor;
        let selected = ed.selected_text();
        let initial =
            if !selected.is_empty() && !selected.contains('\n') { selected } else { self.last_query.clone() };
        let anchor = ed.primary().start();
        match &mut self.panel {
            Some(Panel::Find { query, replace, in_replace, .. }) => {
                *query = LineInput::new(&initial);
                *in_replace = false;
                if with_replace && replace.is_none() {
                    *replace = Some(LineInput::new(&self.last_replace));
                }
            }
            _ => {
                self.close_panel(false);
                let replace = with_replace.then(|| LineInput::new(&self.last_replace));
                self.panel =
                    Some(Panel::Find { query: LineInput::new(&initial), replace, in_replace: false, anchor });
            }
        }
        self.panel_focus = true;
        self.window.request_redraw();
    }

    fn open_goto(&mut self) {
        self.close_panel(false);
        let tab = self.tab();
        let origin_scroll = (tab.scroll_x, tab.top_line, tab.top_offset);
        self.panel =
            Some(Panel::GoTo { input: LineInput::new(""), origin: tab.editor.primary(), origin_scroll });
        self.panel_focus = true;
        self.window.request_redraw();
    }

    /// Closes the panel. Cancelling go-to-line returns to where it started.
    fn close_panel(&mut self, cancel: bool) {
        match self.panel.take() {
            Some(Panel::Find { query, replace, .. }) => {
                self.last_query = query.text;
                if let Some(replace) = replace {
                    self.last_replace = replace.text;
                }
            }
            Some(Panel::GoTo { origin, origin_scroll, .. }) if cancel => {
                let tab = self.tab_mut();
                tab.editor.set_selection(origin);
                (tab.scroll_x, tab.top_line, tab.top_offset) = origin_scroll;
            }
            _ => {}
        }
        self.panel_focus = false;
        self.window.request_redraw();
    }

    /// Live preview as the panel's text changes: select the first match, or jump to the line.
    fn panel_changed(&mut self, fonts: &mut Fonts) {
        match &self.panel {
            Some(Panel::Find { query, anchor, in_replace: false, .. }) => {
                // The anchor may come from another tab's text; keep it in range.
                let (query, anchor) = (query.text.clone(), (*anchor).min(self.tab().editor.len()));
                if let Ok(matches) = self.matches(&query) {
                    let sel = match find::next_match(&matches, anchor) {
                        Some((start, end)) => Selection { anchor: start, head: end },
                        None => Selection::caret(anchor),
                    };
                    self.ed().set_selection(sel);
                    self.reveal(fonts);
                }
            }
            Some(Panel::GoTo { input, .. }) => {
                let mut parts = input.text.split(':').map(|p| p.trim().parse::<usize>().ok());
                let (Some(Some(line)), column) = (parts.next(), parts.next().flatten()) else { return };
                let ed = self.ed();
                let line = line.clamp(1, ed.doc.text.len_lines()) - 1;
                let start = ed.doc.text.line_to_char(line);
                let column = column.unwrap_or(1).max(1) - 1;
                let pos = (start + column).min(ed.line_end(line));
                ed.set_selection(Selection::caret(pos));
                // Centre the target line.
                let space = self.text_height() / 2.0;
                self.scroll_to_row(fonts, line, 0, space);
                self.reveal(fonts);
            }
            _ => {}
        }
    }

    fn query(&self) -> String {
        match &self.panel {
            Some(Panel::Find { query, .. }) => query.text.clone(),
            _ => self.last_query.clone(),
        }
    }

    /// Matches for `query` in the active tab, cached until the text or options change.
    fn matches(&mut self, query: &str) -> Result<Rc<Vec<(usize, usize)>>, String> {
        let options = self.find_options;
        let tab = self.tab_mut();
        let version = tab.editor.doc.version;
        if let Some(c) = &tab.matches {
            if c.version == version && c.query == query && c.options == options {
                return c.result.clone();
            }
        }
        let result = Search::new(query, options).map(|search| {
            Rc::new(search.map_or_else(Vec::new, |s| s.find_all(&tab.editor.doc.text.to_string())))
        });
        tab.matches = Some(MatchCache { version, query: query.to_string(), options, result: result.clone() });
        result
    }

    fn find_next(&mut self, fonts: &mut Fonts, backwards: bool) {
        let query = self.query();
        if query.is_empty() {
            return self.open_find(false);
        }
        let matches = match self.matches(&query) {
            Ok(m) => m,
            Err(e) => return self.notify(&e),
        };
        let sel = self.tab().editor.primary();
        let found = if backwards {
            find::prev_match(&matches, sel.start())
        } else {
            find::next_match(&matches, sel.end())
        };
        match found {
            Some((start, end)) => {
                self.ed().set_selection(Selection { anchor: start, head: end });
                self.tab_mut().goals.clear();
                self.reveal(fonts);
            }
            None => self.notify(&format!("No matches for \"{query}\"")),
        }
        self.window.request_redraw();
    }

    fn replacement_text(&self) -> String {
        match &self.panel {
            Some(Panel::Find { replace: Some(r), .. }) => r.text.clone(),
            _ => self.last_replace.clone(),
        }
    }

    /// Replaces the selection if it is a match, then selects the next match.
    fn replace_next(&mut self, fonts: &mut Fonts) {
        let (query, with) = (self.query(), self.replacement_text());
        let search = match Search::new(&query, self.find_options) {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(e) => return self.notify(&e),
        };
        let ed = &self.tab().editor;
        let sel = ed.primary();
        if !sel.is_empty() {
            let text = ed.doc.text.to_string();
            let start = ed.doc.text.char_to_byte(sel.start());
            let end = ed.doc.text.char_to_byte(sel.end());
            if let Some(replacement) = search.replacement_at(&text, start, end, &with) {
                self.ed().replace_selection(&replacement);
                self.after_command(fonts);
            }
        }
        self.find_next(fonts, false);
    }

    fn replace_all(&mut self, fonts: &mut Fonts) {
        let (query, with) = (self.query(), self.replacement_text());
        let search = match Search::new(&query, self.find_options) {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(e) => return self.notify(&e),
        };
        let edits = search.replace_all(&self.tab().editor.doc.text.to_string(), &with);
        let count = edits.len();
        self.ed().replace_many(edits);
        self.after_command(fonts);
        self.notify(&format!("Replaced {count} occurrence{}", if count == 1 { "" } else { "s" }));
    }

    fn toggle(&mut self, fonts: &mut Fonts, toggle: Toggle) {
        let o = &mut self.find_options;
        match toggle {
            Toggle::Case => o.case_sensitive = !o.case_sensitive,
            Toggle::Regex => o.regex = !o.regex,
            Toggle::Word => o.whole_word = !o.whole_word,
        }
        self.panel_changed(fonts);
        self.window.request_redraw();
    }

    /// Saves the active tab, formatting it first when `format_on_save` is on.
    fn save(&mut self) {
        let path = self.tab().editor.doc.path.clone();
        match path {
            None => self.save_as(),
            Some(path) if self.settings.format_on_save && self.settings.formatter_for(&path).is_some() => {
                self.format(true)
            }
            Some(_) => self.save_now(),
        }
    }

    fn save_now(&mut self) {
        match self.ed().doc.save() {
            Ok(()) => {
                self.tab_mut().close_armed = None;
                self.close_armed = None;
                self.session_changed = true;
                let path = self.tab().editor.doc.path.clone();
                if path.is_some() && path == settings::path() {
                    self.settings_saved = true;
                }
                self.notify("Saved");
            }
            Err(e) => self.notify(&format!("Save failed: {e}")),
        }
    }

    fn open_settings(&mut self) {
        match settings::ensure_file() {
            Ok(path) => {
                self.open_paths(vec![path]);
            }
            Err(e) => self.notify(&format!("Could not create the settings file: {e}")),
        }
    }

    /// Whether the settings file was saved here since the last call; clears the flag.
    pub fn take_settings_saved(&mut self) -> bool {
        std::mem::take(&mut self.settings_saved)
    }

    /// Applies reloaded settings: fonts, theme, and tab width. Indentation of open files
    /// is left as it was detected.
    pub fn apply_settings(
        &mut self,
        fonts: &mut Fonts,
        settings: Rc<Settings>,
        theme: Theme,
        palette: Palette,
    ) {
        self.font_points = settings.font_size;
        self.settings = settings;
        self.palette = palette;
        self.theme = theme;
        let window_id = self.window.id();
        for tab in &mut self.tabs {
            let (proxy, id) = (self.proxy.clone(), tab.id);
            tab.highlighter = Highlighter::spawn(self.theme.clone(), move |batch| {
                let _ = proxy.send_event(UserEvent::Highlighted(window_id, id, batch));
            });
            let doc = &tab.editor.doc;
            tab.highlighter.reset(doc.version, doc.text.clone(), doc.path.clone());
            tab.styles = StyleCache::new(doc.text.len_lines());
        }
        self.rebuild_layout(fonts);
    }

    /// Runs the active tab's formatter on a background thread; `then_save` saves after.
    fn format(&mut self, then_save: bool) {
        let tab = self.tab();
        let Some(path) = tab.editor.doc.path.clone() else {
            return self.notify("Save the file first, so oxide knows which formatter to use");
        };
        let Some(formatter) = self.settings.formatter_for(&path).cloned() else {
            if then_save {
                return self.save_now();
            }
            let name = path.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
            return self.notify(&format!("No formatter for {name}. Add one in settings ({PRIMARY}+,)"));
        };
        let (text, version, tab_id) = (tab.editor.doc.text.to_string(), tab.editor.doc.version, tab.id);
        let (proxy, window) = (self.proxy.clone(), self.window.id());
        self.notify(&format!("Formatting with {}…", formatter.command[0]));
        std::thread::spawn(move || {
            let result = format::run(&formatter.command, &text, Some(&path));
            let _ =
                proxy.send_event(UserEvent::Formatted { window, tab: tab_id, version, result, then_save });
        });
    }

    /// Applies a formatter's output as one undo step, if the text hasn't changed since.
    pub fn apply_format(
        &mut self,
        fonts: &mut Fonts,
        tab_id: u64,
        version: u64,
        result: Result<String, String>,
        then_save: bool,
    ) {
        let Some(index) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        let previous = std::mem::replace(&mut self.active, index);
        if self.tab().editor.doc.version != version {
            self.notify("The file changed while it was being formatted; format again");
        } else {
            match result {
                Ok(formatted) => {
                    let ed = &self.tab().editor;
                    if let Some(edit) = format::minimal_edit(&ed.doc.text.to_string(), &formatted) {
                        // Keep the caret on the same line and column.
                        let line = ed.caret_line();
                        let column = ed.primary().head - ed.doc.text.line_to_char(line);
                        self.ed().replace_many(vec![edit]);
                        let ed = self.ed();
                        let line = line.min(ed.doc.text.len_lines() - 1);
                        let pos = (ed.doc.text.line_to_char(line) + column).min(ed.line_end(line));
                        ed.set_selection(Selection::caret(pos));
                        self.after_command(fonts);
                    }
                    if then_save {
                        self.save_now();
                    } else {
                        self.notify("Formatted");
                    }
                }
                Err(e) if then_save => {
                    // A failing formatter must never stop a save.
                    self.save_now();
                    self.notify(&format!("Saved without formatting. {e}"));
                }
                Err(e) => self.notify(&format!("Format failed. {e}")),
            }
        }
        self.active = if previous < self.tabs.len() { previous } else { index };
        self.window.request_redraw();
    }

    fn save_as(&mut self) {
        let current = self.tab().editor.doc.path.clone();
        let mut dialog = rfd::FileDialog::new().set_parent(&*self.window).set_file_name(self.tab().name());
        if let Some(dir) = current.as_deref().and_then(Path::parent) {
            dialog = dialog.set_directory(dir);
        }
        let Some(path) = dialog.save_file() else { return };
        self.ed().doc.path = Some(path);
        self.save_now();
        // The new name may mean a different language.
        let tab = self.tab_mut();
        let doc = &tab.editor.doc;
        tab.highlighter.reset(doc.version, doc.text.clone(), doc.path.clone());
        tab.styles = StyleCache::new(doc.text.len_lines());
        self.update_title();
    }

    fn open_dialog(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_parent(&*self.window);
        if let Some(dir) = self.tab().editor.doc.path.as_deref().and_then(Path::parent) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(paths) = dialog.pick_files() {
            self.open_paths(paths);
        }
    }

    fn zoom(&mut self, fonts: &mut Fonts, delta: f32) {
        self.font_points = (self.font_points + delta).clamp(6.0, 48.0);
        self.rebuild_layout(fonts);
    }

    /// Re-lays out everything after the font database changed.
    pub fn fonts_changed(&mut self, fonts: &mut Fonts) {
        self.rebuild_layout(fonts);
    }

    fn rebuild_layout(&mut self, fonts: &mut Fonts) {
        let size = self.font_points * self.window.scale_factor() as f32;
        self.layout = TextLayout::new(fonts, size, self.settings.tab_size as u16);
        self.window.request_redraw();
    }

    /// Moves every caret up or down by `rows` screen rows, keeping each one's column.
    fn move_vertical(&mut self, fonts: &mut Fonts, rows: isize, extend: bool) {
        let sels = self.tab().editor.sels().to_vec();
        if self.tab().goals.len() != sels.len() {
            let goals = sels.iter().map(|s| self.locate(fonts, s.head).2).collect();
            self.tab_mut().goals = goals;
        }
        let goals = self.tab().goals.clone();
        let len = self.tab().editor.len();
        let heads: Vec<usize> = sels
            .iter()
            .zip(&goals)
            .map(|(s, &x)| {
                let (line, row, _) = self.locate(fonts, s.head);
                match self.step_rows(fonts, line, row, rows) {
                    Some((line, row)) => self.pos_in_row(fonts, line, row, x),
                    None if rows < 0 => 0,
                    None => len,
                }
            })
            .collect();
        self.ed().set_heads(&heads, extend);
        // Carets that met were merged; their goals no longer line up.
        if self.tab().editor.sels().len() != goals.len() {
            self.tab_mut().goals.clear();
        }
    }

    /// Adds a caret on the row above the first selection, or below the last.
    fn add_caret_vertically(&mut self, fonts: &mut Fonts, below: bool) {
        let sels = self.tab().editor.sels();
        let edge = if below { *sels.last().unwrap() } else { sels[0] };
        let (line, row, x) = self.locate(fonts, edge.head);
        if let Some((line, row)) = self.step_rows(fonts, line, row, if below { 1 } else { -1 }) {
            let pos = self.pos_in_row(fonts, line, row, x);
            self.ed().add_selection(Selection::caret(pos));
        }
    }

    fn click(&mut self, fonts: &mut Fonts, mods: ModifiersState) -> Action {
        match self.region_at(self.mouse) {
            Some(Hit::Tab(i)) => {
                self.switch_tab(i);
                return Action::None;
            }
            Some(Hit::CloseTab(i)) => return self.close_tab(i),
            Some(Hit::Toggle(t)) => {
                self.toggle(fonts, t);
                return Action::None;
            }
            Some(Hit::PanelRow(row)) => {
                if let Some(Panel::Find { in_replace, .. }) = &mut self.panel {
                    *in_replace = row == 1;
                }
                self.panel_focus = true;
                self.window.request_redraw();
                return Action::None;
            }
            None => {}
        }
        let y = self.mouse.1;
        if y < self.text_top() || y >= self.text_bottom() {
            return Action::None;
        }
        self.panel_focus = false;
        let now = Instant::now();
        let count = match self.last_click {
            Some((at, (x, y), n))
                if now.duration_since(at) < MULTI_CLICK
                    && (x - self.mouse.0).abs() < 6.0
                    && (y - self.mouse.1).abs() < 6.0 =>
            {
                n % 3 + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, self.mouse, count));
        let pos = self.hit_text(fonts);
        let add_caret = if MAC { mods.super_key() } else { mods.control_key() };
        match count {
            1 if add_caret => {
                self.ed().add_selection(Selection::caret(pos));
                self.last_click = None;
            }
            1 => self.ed().move_to(pos, mods.shift_key()),
            2 => self.ed().select_word_at(pos),
            _ => self.ed().select_line_at(pos),
        }
        self.dragging = count == 1 && !add_caret;
        self.tab_mut().goals.clear();
        self.after_command(fonts);
        Action::None
    }

    /// Scrolls by pixels. The view is anchored to its top line, so wrapped lines above it
    /// never shift what is on screen.
    fn scroll_by(&mut self, fonts: &mut Fonts, dx: f32, dy: f32) {
        let line_height = self.line_height();
        let last = self.tab().editor.doc.text.len_lines() - 1;
        let (mut line, mut offset) = (self.tab().top_line.min(last), self.tab().top_offset + dy);
        while offset < 0.0 {
            if line == 0 {
                offset = 0.0;
                break;
            }
            line -= 1;
            offset += self.row_count(fonts, line) as f32 * line_height;
        }
        loop {
            let height = self.row_count(fonts, line) as f32 * line_height;
            if line == last {
                // The last line may scroll up to the top of the view, no further.
                offset = offset.min(height - line_height);
                break;
            }
            if offset < height {
                break;
            }
            offset -= height;
            line += 1;
        }
        let wrap = self.tab().wrap;
        let tab = self.tab_mut();
        (tab.top_line, tab.top_offset) = (line, offset.max(0.0));
        tab.scroll_x = if wrap { 0.0 } else { (tab.scroll_x + dx).max(0.0) };
        self.window.request_redraw();
    }

    /// Scrolls just enough to bring the primary caret into view.
    fn reveal(&mut self, fonts: &mut Fonts) {
        let line_height = self.line_height();
        let height = self.text_height();
        let (line, row, x) = self.locate(fonts, self.tab().editor.primary().head);
        let row_top = row as f32 * line_height;
        let (top_line, top_offset) = (self.tab().top_line, self.tab().top_offset);
        if line < top_line || (line == top_line && row_top < top_offset) {
            self.scroll_to_row(fonts, line, row, 0.0);
        } else {
            // Walk down from the top of the view, stopping once past its bottom.
            let (mut y, mut l) = (-top_offset, top_line);
            while l < line && y <= height {
                y += self.row_count(fonts, l) as f32 * line_height;
                l += 1;
            }
            if l < line || y + row_top + line_height > height {
                self.scroll_to_row(fonts, line, row, height - line_height);
            }
        }
        if !self.tab().wrap {
            let width = self.window.inner_size().width as f32 - self.text_left();
            let margin = self.char_width() * 4.0;
            let tab = self.tab_mut();
            if x < tab.scroll_x {
                tab.scroll_x = (x - width / 3.0).max(0.0);
            } else if x > tab.scroll_x + width - margin {
                tab.scroll_x = x - width + margin;
            }
        }
    }

    /// Hands new edits to the highlighter and keeps the colour cache aligned with the text.
    fn after_command(&mut self, fonts: &mut Fonts) {
        let tab = self.tab_mut();
        let changes = std::mem::take(&mut tab.editor.changes);
        if !changes.is_empty() {
            let version = tab.editor.doc.version;
            tab.styles.record(version, &changes);
            tab.highlighter.edit(version, tab.editor.doc.text.clone(), changes);
            tab.close_armed = None;
            self.close_armed = None;
            self.session_changed = true;
        }
        self.reveal(fonts);
        self.update_title();
        self.window.request_redraw();
    }

    pub fn apply_highlights(&mut self, tab_id: u64, mut batch: Batch) {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) else { return };
        if let Some(name) = batch.syntax_name.take() {
            tab.editor.pairs = if self.settings.auto_pairs { editor::pairs_for(&name) } else { &[] };
            tab.syntax_name = name;
        }
        tab.styles.apply(batch);
        self.window.request_redraw();
    }

    pub fn draw(&mut self, fonts: &mut Fonts) {
        let size = self.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        let (width, height) = (size.width as f32, size.height as f32);
        let line_height = self.line_height();
        let baseline = self.layout.metrics.baseline;
        let char_width = self.char_width();
        let gutter_width = self.gutter_width();
        let text_left = self.text_left();
        let text_top = self.text_top();
        let text_bottom = self.text_bottom();
        let pad = (char_width * 1.5).round();
        let caret_width = (self.window.scale_factor() as f32 * 2.0).round();
        let bar = self.tab_bar_height();
        let row_height = self.panel_row_height();
        let status_height = self.status_height();
        let p = self.palette;

        let line_count = self.tab().editor.doc.text.len_lines();
        let top_line = self.tab().top_line.min(line_count - 1);
        let top_offset = self.tab().top_offset;
        let wrapping = self.tab().wrap;
        let sels: Vec<Selection> = self.tab().editor.sels().to_vec();
        let sel = self.tab().editor.primary();
        let caret_line = self.tab().editor.caret_line();
        let caret_on = self.caret_visible();

        // Lay out the visible lines first; painting below only borrows the results.
        let mut lines = Vec::new();
        let mut widest: f32 = 0.0;
        let (mut y, mut line) = (text_top - top_offset, top_line);
        while line < line_count && y < text_bottom {
            let (text, laid) = self.layout_rows(fonts, line);
            widest = widest.max(laid.width());
            let line_top = y;
            y += laid.rows().len() as f32 * line_height;
            lines.push((line, line_top, text, laid));
            line += 1;
        }
        let max_scroll_x = (widest - (width - text_left) / 2.0).max(0.0);
        let scroll_x = if wrapping { 0.0 } else { self.tab().scroll_x.min(max_scroll_x) };
        self.tab_mut().scroll_x = scroll_x;
        let x0 = text_left - scroll_x;

        let finding = matches!(self.panel, Some(Panel::Find { .. }));
        let query = self.query();
        let matches = if finding { self.matches(&query) } else { Ok(Rc::new(Vec::new())) };
        let preedit =
            self.preedit.clone().map(|text| self.layout.line(fonts, &text, &[], p.foreground, false));

        if self.surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buffer) = self.surface.buffer_mut() else { return };
        let mut canvas =
            Canvas { pixels: &mut buffer, width: size.width as usize, height: size.height as usize };
        canvas.fill(0.0, 0.0, width, height, p.background);
        let mut regions = Vec::new();
        let tab = &self.tabs[self.active];
        let ed = &tab.editor;
        let mut caret_rect = None;

        for (line, top, text, laid) in &lines {
            let (line, top) = (*line, *top);
            let rows = laid.rows();
            let row_top = |row: usize| top + row as f32 * line_height;
            let line_start = ed.doc.text.line_to_char(line);
            let line_end = ed.line_end(line);
            let byte = |pos: usize| char_to_byte(text, pos - line_start);
            // Selections that touch this line, found by binary search (there may be many).
            let from = sels.partition_point(|s| s.end() < line_start);
            let here: Vec<Selection> =
                sels[from..].iter().take_while(|s| s.start() <= line_end).copied().collect();
            let caret_here = |s: &Selection| s.head >= line_start && s.head <= line_end;
            if here.iter().any(|s| s.is_empty() && caret_here(s)) {
                canvas.fill(0.0, top, width, row_top(rows.len()), p.line_highlight);
            }

            if let Ok(matches) = &matches {
                let from = matches.partition_point(|m| m.1 < line_start);
                for &(start, end) in matches[from..].iter().take_while(|m| m.0 <= line_end) {
                    let (a, b) = (byte(start.max(line_start)), byte(end.min(line_end)));
                    for (row, xa, xb) in row_spans(laid, text.len(), a, b) {
                        let (y0, y1) = (row_top(row) + 1.0, row_top(row + 1) - 1.0);
                        let thickness = (caret_width / 2.0).max(1.0);
                        let (xa, xb) = ((x0 + xa).max(text_left), (x0 + xb).max(text_left));
                        canvas.outline(xa, y0, xb, y1, thickness, p.find_highlight);
                    }
                }
            }
            for s in here.iter().filter(|s| !s.is_empty()) {
                let (a, b) = (byte(s.start().max(line_start)), byte(s.end().min(line_end)));
                for (row, xa, xb) in row_spans(laid, text.len(), a, b) {
                    let (xa, xb) = ((x0 + xa).max(text_left), (x0 + xb).max(text_left));
                    canvas.fill(xa, row_top(row), xb, row_top(row + 1), p.selection);
                }
                if s.end() > line_end {
                    // The selected line break, after the last row's text.
                    let last = rows.len() - 1;
                    let x = x0 + x_for_byte(&rows[last], text.len());
                    canvas.fill(
                        x.max(text_left),
                        row_top(last),
                        x + char_width * 0.6,
                        row_top(last + 1),
                        p.selection,
                    );
                }
            }

            for (row, laid_row) in rows.iter().enumerate() {
                canvas.text(fonts, laid_row, x0, row_top(row) + baseline, text_left, width);
            }

            let color = if here.iter().any(caret_here) { p.gutter_active } else { p.gutter };
            let number = self.layout.line(fonts, &(line + 1).to_string(), &[], color, false);
            let nx = gutter_width - pad - number.w;
            canvas.text(fonts, &number, nx, top + baseline, 0.0, gutter_width);

            for s in here.iter().filter(|s| caret_here(s)) {
                let b = byte(s.head);
                let row = laid.row_of(b);
                let (x, top) = (x0 + x_for_byte(&rows[row], b), row_top(row));
                let is_primary = *s == sel;
                if is_primary {
                    caret_rect = Some((x, top));
                }
                match &preedit {
                    // Composition text floats over the line at the caret, underlined.
                    Some(preedit) if is_primary => {
                        canvas.fill(x, top, x + preedit.w, top + line_height, p.background);
                        canvas.text(fonts, preedit, x, top + baseline, text_left, width);
                        let underline = top + baseline + 2.0;
                        canvas.fill(x, underline, x + preedit.w, underline + 1.0, p.foreground);
                    }
                    _ if x >= text_left - 1.0 && !self.panel_focus && caret_on => {
                        canvas.fill(x, top, x + caret_width, top + line_height, p.caret);
                    }
                    _ => {}
                }
            }
        }

        // Tab bar.
        canvas.fill(0.0, 0.0, width, bar, p.chrome);
        let close = self.layout.line(fonts, "×", &[], p.chrome_text, true);
        let labels: Vec<_> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let dirty = if t.editor.doc.is_dirty() { " •" } else { "" };
                let color = if i == self.active { p.foreground } else { p.chrome_text };
                self.layout.line(fonts, &format!("{}{dirty}", t.name()), &[], color, true)
            })
            .collect();
        let natural: Vec<f32> = labels
            .iter()
            .map(|l| (l.w + pad * 3.0 + close.w).clamp(char_width * 8.0, char_width * 28.0))
            .collect();
        let total: f32 = natural.iter().sum();
        let squeeze = if total > width { width / total } else { 1.0 };
        let text_y = ((bar - line_height) / 2.0).round() + baseline;
        let mut x = 0.0;
        for (i, label) in labels.iter().enumerate() {
            let x1 = x + (natural[i] * squeeze).floor();
            let close_x = x1 - pad - close.w;
            if i == self.active {
                canvas.fill(x, 0.0, x1, bar, p.background);
            } else if i + 1 != self.active && i + 1 < labels.len() {
                canvas.fill(x1 - 1.0, bar * 0.25, x1, bar * 0.75, p.gutter);
            }
            canvas.text(fonts, label, x + pad, text_y, x, close_x - pad / 2.0);
            canvas.text(fonts, &close, close_x, text_y, x, x1);
            regions.push(Region { x0: x, y0: 0.0, x1, y1: bar, hit: Hit::Tab(i) });
            regions.push(Region { x0: close_x - pad / 2.0, y0: 0.0, x1, y1: bar, hit: Hit::CloseTab(i) });
            x = x1;
        }

        // Find / go-to bar.
        let rows: Vec<(&str, &LineInput, bool)> = match &self.panel {
            None => Vec::new(),
            Some(Panel::Find { query, replace, in_replace, .. }) => {
                let mut rows = vec![("Find", query, !in_replace)];
                if let Some(replace) = replace {
                    rows.push(("Replace", replace, *in_replace));
                }
                rows
            }
            Some(Panel::GoTo { input, .. }) => vec![("Go to line", input, true)],
        };
        let row_labels: Vec<_> =
            rows.iter().map(|(label, ..)| self.layout.line(fonts, label, &[], p.chrome_text, true)).collect();
        let label_width = row_labels.iter().map(|l| l.w).fold(0.0, f32::max);
        for (row, ((_, input, focused), label)) in rows.iter().zip(&row_labels).enumerate() {
            let top = text_bottom + row as f32 * row_height;
            canvas.fill(0.0, top, width, top + row_height, p.panel);
            let text_y = top + ((row_height - line_height) / 2.0).round() + baseline;
            canvas.text(fonts, label, pad, text_y, 0.0, width);

            let mut right = width - pad;
            if row == 0 && finding {
                let o = self.find_options;
                let toggles = [
                    ("\\b", o.whole_word, Toggle::Word),
                    (".*", o.regex, Toggle::Regex),
                    ("Aa", o.case_sensitive, Toggle::Case),
                ];
                for (glyph, on, toggle) in toggles {
                    let color = if on { p.foreground } else { p.gutter };
                    let laid = self.layout.line(fonts, glyph, &[], color, false);
                    let (bx0, bx1) = (right - laid.w - pad, right);
                    if on {
                        canvas.fill(bx0, top + 3.0, bx1, top + row_height - 3.0, p.selection);
                    }
                    canvas.text(fonts, &laid, bx0 + pad / 2.0, text_y, bx0, bx1);
                    regions.push(Region {
                        x0: bx0,
                        y0: top,
                        x1: bx1,
                        y1: top + row_height,
                        hit: Hit::Toggle(toggle),
                    });
                    right = bx0 - pad / 3.0;
                }
                let count = match &matches {
                    _ if query.is_empty() => String::new(),
                    Err(e) => e.clone(),
                    Ok(m) if m.is_empty() => "No results".into(),
                    Ok(m) => {
                        let total = if m.len() >= MATCH_LIMIT {
                            format!("{MATCH_LIMIT}+")
                        } else {
                            m.len().to_string()
                        };
                        match m.binary_search(&(sel.start(), sel.end())) {
                            Ok(i) => format!("{} of {total}", i + 1),
                            Err(_) => format!("{total} matches"),
                        }
                    }
                };
                let laid = self.layout.line(fonts, &count, &[], p.chrome_text, true);
                right -= laid.w + pad;
                canvas.text(fonts, &laid, right + pad / 2.0, text_y, right, width);
            }

            let (bx0, bx1) = (pad * 2.0 + label_width, right - pad / 2.0);
            canvas.fill(bx0, top + 3.0, bx1, top + row_height - 3.0, p.background);
            let laid = self.layout.line(fonts, &input.text, &[], p.foreground, false);
            let tx = bx0 + pad / 2.0;
            if input.all_selected {
                canvas.fill(tx, top + 5.0, tx + laid.w, top + row_height - 5.0, p.selection);
            }
            canvas.text(fonts, &laid, tx, text_y, bx0, bx1);
            if *focused && self.panel_focus {
                let cx = tx + x_for_byte(&laid, input.cursor);
                canvas.fill(cx, top + 5.0, cx + caret_width, top + row_height - 5.0, p.caret);
                caret_rect = Some((cx, top));
            }
            regions.push(Region { x0: 0.0, y0: top, x1: bx1, y1: top + row_height, hit: Hit::PanelRow(row) });
        }

        // Status bar.
        let top = height - status_height;
        canvas.fill(0.0, top, width, height, p.chrome);
        let line_start = ed.doc.text.line_to_char(caret_line);
        let mut left = format!("Line {}, Column {}", caret_line + 1, sel.head - line_start + 1);
        if sels.len() > 1 {
            left.push_str(&format!("  ({} selections)", sels.len()));
        }
        if let Some((message, at)) = &self.message {
            if at.elapsed() < MESSAGE_TIME {
                left.push_str("    ");
                left.push_str(message);
            }
        }
        let indent = match ed.indent {
            Indent::Tabs => "Tabs".to_string(),
            Indent::Spaces(n) => format!("Spaces: {n}"),
        };
        let right = format!("{indent}      {}", tab.syntax_name);
        let text_y = top + ((status_height - line_height) / 2.0).round() + baseline;
        let left = self.layout.line(fonts, &left, &[], p.chrome_text, true);
        canvas.text(fonts, &left, pad, text_y, 0.0, width);
        let right = self.layout.line(fonts, &right, &[], p.chrome_text, true);
        canvas.text(fonts, &right, width - pad - right.w, text_y, 0.0, width);

        if let Some(path) = std::env::var_os("OXIDE_SNAPSHOT") {
            let _ =
                crate::render::write_bmp(path.as_ref(), &buffer, size.width as usize, size.height as usize);
        }
        let _ = buffer.present();
        self.regions = regions;
        self.layout.end_frame();

        // Tell the input method where the caret is, so its candidate window sits beside it.
        if let Some((x, y)) = caret_rect.map(|(x, y)| (x as i32, y as i32)) {
            if self.ime_area != Some((x, y)) {
                self.ime_area = Some((x, y));
                self.window.set_ime_cursor_area(
                    PhysicalPosition::new(x, y),
                    PhysicalSize::new(char_width as u32, line_height as u32),
                );
            }
        }
    }
}

/// The parts of bytes `a..b` on each row of `laid`, as `(row, x0, x1)`.
fn row_spans(laid: &Laid, text_len: usize, a: usize, b: usize) -> Vec<(usize, f32, f32)> {
    let rows = laid.rows();
    let start_of = |r: usize| {
        if r == 0 {
            0
        } else {
            rows.get(r).and_then(|row| row.glyphs.iter().map(|g| g.start).min()).unwrap_or(text_len)
        }
    };
    (0..rows.len())
        .filter_map(|r| {
            let (s, e) =
                (a.max(start_of(r)), b.min(if r + 1 < rows.len() { start_of(r + 1) } else { text_len }));
            (s < e).then(|| (r, x_for_byte(&rows[r], s), x_for_byte(&rows[r], e)))
        })
        .collect()
}

/// Text a key press should type, if any. Shortcuts and control characters type nothing.
fn typed_text(event: &KeyEvent, mods: ModifiersState) -> Option<String> {
    // Ctrl+Alt is AltGr on Windows and Linux layouts, which does type characters.
    let shortcut =
        if MAC { mods.super_key() || mods.control_key() } else { mods.control_key() && !mods.alt_key() };
    let text = event.text.as_ref()?;
    (!shortcut && !text.is_empty() && !text.chars().any(char::is_control)).then(|| text.to_string())
}

fn char_to_byte(text: &str, chars: usize) -> usize {
    text.char_indices().nth(chars).map_or(text.len(), |(i, _)| i)
}
