//! One editor window: input handling, scrolling, and painting a frame.

use crate::document::{Document, Selection};
use crate::editor::{Editor, Indent};
use crate::highlight::{Batch, Highlighter, StyleCache};
use crate::render::{blend, byte_for_x, x_for_byte, Canvas, Fonts, TextLayout};
use crate::UserEvent;
use cosmic_text::LayoutLine;
use std::error::Error;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};
use syntect::highlighting::{Color, Theme};
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

const FONT_POINTS: f32 = 14.0;
const MULTI_CLICK: Duration = Duration::from_millis(400);
const MESSAGE_TIME: Duration = Duration::from_secs(4);
const MAC: bool = cfg!(target_os = "macos");

/// What the app must do after a window handled an event.
pub enum Action {
    None,
    Close,
    Quit,
    Copy(String),
    Paste,
}

#[derive(Clone, Copy)]
pub struct Palette {
    background: u32,
    foreground: u32,
    caret: u32,
    selection: u32,
    line_highlight: u32,
    gutter: u32,
    gutter_active: u32,
    status_background: u32,
    status_foreground: u32,
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
            gutter: over(s.gutter_foreground, 90),
            gutter_active: blend(background, foreground, 210),
            status_background: blend(background, 0x000000, 70),
            status_foreground: blend(background, foreground, 190),
        }
    }
}

pub struct Win {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    editor: Editor,
    layout: TextLayout,
    palette: Palette,
    highlighter: Highlighter,
    styles: StyleCache,
    syntax_name: String,
    scroll_x: f32,
    scroll_y: f32,
    /// Remembered x for up/down so the caret keeps its column across short lines.
    goal_x: Option<f32>,
    mouse: (f32, f32),
    dragging: bool,
    last_click: Option<(Instant, (f32, f32), u32)>,
    message: Option<(String, Instant)>,
    close_armed: Option<Instant>,
    font_points: f32,
}

impl Win {
    pub fn new(
        event_loop: &ActiveEventLoop,
        fonts: &mut Fonts,
        doc: Document,
        theme: Theme,
        palette: Palette,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<Self, Box<dyn Error>> {
        let attrs = Window::default_attributes()
            .with_title("oxide")
            .with_inner_size(LogicalSize::new(960.0, 680.0))
            .with_visible(false);
        let window = Rc::new(event_loop.create_window(attrs)?);
        let context = softbuffer::Context::new(window.clone())?;
        let surface = softbuffer::Surface::new(&context, window.clone())?;
        let layout = TextLayout::new(fonts, FONT_POINTS * window.scale_factor() as f32);

        let id = window.id();
        let highlighter = Highlighter::spawn(theme, move |batch| {
            let _ = proxy.send_event(UserEvent::Highlighted(id, batch));
        });
        highlighter.reset(doc.version, doc.text.clone(), doc.path.clone());

        let read_only = doc.read_only;
        let mut win = Self {
            window,
            surface,
            styles: StyleCache::new(doc.text.len_lines()),
            editor: Editor::new(doc),
            layout,
            palette,
            highlighter,
            syntax_name: String::new(),
            scroll_x: 0.0,
            scroll_y: 0.0,
            goal_x: None,
            mouse: (0.0, 0.0),
            dragging: false,
            last_click: None,
            message: None,
            close_armed: None,
            font_points: FONT_POINTS,
        };
        if read_only {
            win.notify("Not valid UTF-8: opened read-only");
        }
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

    pub fn is_dirty(&self) -> bool {
        self.editor.doc.is_dirty()
    }

    pub fn notify(&mut self, message: &str) {
        self.message = Some((message.to_string(), Instant::now()));
        self.window.request_redraw();
    }

    /// True if the window may close now; otherwise warns about unsaved changes.
    pub fn confirm_close(&mut self, window: Duration) -> bool {
        if !self.is_dirty() || self.close_armed.is_some_and(|t| t.elapsed() < window) {
            return true;
        }
        self.close_armed = Some(Instant::now());
        let save = if MAC { "Cmd+S" } else { "Ctrl+S" };
        self.notify(&format!("Unsaved changes. Close again to discard, or {save} to save."));
        false
    }

    fn update_title(&self) {
        let name = self
            .editor
            .doc
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map_or("untitled".into(), |n| n.to_string_lossy());
        let dirty = self.is_dirty();
        self.window.set_title(&format!("{name}{}", if dirty { " •" } else { "" }));
        #[cfg(target_os = "macos")]
        {
            use winit::platform::macos::WindowExtMacOS;
            self.window.set_document_edited(dirty);
        }
    }

    fn line_height(&self) -> f32 {
        self.layout.metrics.line_height
    }

    fn gutter_width(&self) -> f32 {
        let digits = self.editor.doc.text.len_lines().to_string().len().max(3);
        (digits as f32 + 3.0) * self.layout.metrics.char_width
    }

    fn text_left(&self) -> f32 {
        self.gutter_width() + self.layout.metrics.char_width
    }

    fn status_height(&self) -> f32 {
        (self.line_height() * 1.25).round()
    }

    fn text_height(&self) -> f32 {
        self.window.inner_size().height as f32 - self.status_height()
    }

    fn layout_line(&mut self, fonts: &mut Fonts, line: usize) -> (String, Rc<LayoutLine>) {
        let text = self.editor.line_text(line);
        let runs = self.styles.get(line);
        let laid =
            self.layout.line(fonts, &text, runs.as_deref().unwrap_or(&[]), self.palette.foreground, false);
        (text, laid)
    }

    fn caret_x(&mut self, fonts: &mut Fonts) -> f32 {
        let line = self.editor.caret_line();
        let column = self.editor.sel.head - self.editor.doc.text.line_to_char(line);
        let (text, laid) = self.layout_line(fonts, line);
        x_for_byte(&laid, char_to_byte(&text, column))
    }

    /// Char index under the mouse pointer.
    fn hit(&mut self, fonts: &mut Fonts) -> usize {
        let (x, y) = self.mouse;
        let last = self.editor.doc.text.len_lines() - 1;
        let line = (((y + self.scroll_y) / self.line_height()).floor().max(0.0) as usize).min(last);
        let (text, laid) = self.layout_line(fonts, line);
        let byte = byte_for_x(&laid, &text, x - self.text_left() + self.scroll_x);
        self.editor.doc.text.line_to_char(line) + text[..byte].chars().count()
    }

    pub fn handle(&mut self, fonts: &mut Fonts, event: WindowEvent, mods: ModifiersState) -> Action {
        match event {
            WindowEvent::Resized(_) => self.window.request_redraw(),
            WindowEvent::ScaleFactorChanged { .. } => self.rebuild_layout(fonts),
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. }
                if event.state == ElementState::Pressed =>
            {
                return self.key(fonts, &event, mods);
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse = (position.x as f32, position.y as f32);
                if self.dragging {
                    let pos = self.hit(fonts);
                    self.editor.move_to(pos, true);
                    self.goal_x = None;
                    self.reveal(fonts);
                    self.window.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed => self.click(fonts, mods.shift_key()),
                ElementState::Released => self.dragging = false,
            },
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => {
                        (x * 3.0 * self.layout.metrics.char_width, y * 3.0 * self.line_height())
                    }
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
                };
                self.scroll_by(-dx, -dy);
            }
            _ => {}
        }
        Action::None
    }

    fn key(&mut self, fonts: &mut Fonts, event: &KeyEvent, mods: ModifiersState) -> Action {
        let shift = mods.shift_key();
        let primary = if MAC { mods.super_key() } else { mods.control_key() };
        let word = if MAC { mods.alt_key() } else { mods.control_key() };
        let mut vertical = false;
        match &event.logical_key {
            Key::Named(key) => match key {
                NamedKey::ArrowLeft if MAC && primary => self.editor.move_line_start(shift),
                NamedKey::ArrowLeft if word => self.editor.move_word_left(shift),
                NamedKey::ArrowLeft => self.editor.move_left(shift),
                NamedKey::ArrowRight if MAC && primary => self.editor.move_line_end(shift),
                NamedKey::ArrowRight if word => self.editor.move_word_right(shift),
                NamedKey::ArrowRight => self.editor.move_right(shift),
                NamedKey::ArrowUp if MAC && primary => self.editor.move_to(0, shift),
                NamedKey::ArrowDown if MAC && primary => {
                    self.editor.move_to(self.editor.len(), shift)
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
                        self.scroll_by(0.0, lines as f32 * self.line_height());
                    }
                    self.move_vertical(fonts, lines, shift);
                    vertical = true;
                }
                NamedKey::Home if !MAC && primary => self.editor.move_to(0, shift),
                NamedKey::End if !MAC && primary => self.editor.move_to(self.editor.len(), shift),
                NamedKey::Home => self.editor.move_line_start(shift),
                NamedKey::End => self.editor.move_line_end(shift),
                NamedKey::Backspace if MAC && primary => self.editor.delete_to_line_start(),
                NamedKey::Backspace if word => self.editor.delete_word_left(),
                NamedKey::Backspace => self.editor.backspace(),
                NamedKey::Delete if word => self.editor.delete_word_right(),
                NamedKey::Delete => self.editor.delete_forward(),
                NamedKey::Enter => self.editor.newline(),
                NamedKey::Tab if shift => self.editor.outdent(),
                NamedKey::Tab => self.editor.indent(),
                NamedKey::Escape => self.editor.sel = Selection::caret(self.editor.sel.head),
                _ => return self.type_text(fonts, event, mods),
            },
            Key::Character(c) if primary || (MAC && mods.control_key()) => {
                match c.to_lowercase().as_str() {
                    "s" => self.save(),
                    "z" if shift => self.editor.redo(),
                    "z" => self.editor.undo(),
                    "y" if !MAC => self.editor.redo(),
                    "a" => self.editor.select_all(),
                    "c" => return Action::Copy(self.editor.copy()),
                    "x" => {
                        let text = self.editor.cut();
                        self.after_command(fonts);
                        return Action::Copy(text);
                    }
                    "v" => return Action::Paste,
                    "w" => return Action::Close,
                    "q" if MAC => return Action::Quit,
                    "=" | "+" => self.zoom(fonts, 1.0),
                    "-" => self.zoom(fonts, -1.0),
                    "0" => self.zoom(fonts, FONT_POINTS - self.font_points),
                    _ => return Action::None,
                }
            }
            _ => return self.type_text(fonts, event, mods),
        }
        if !vertical {
            self.goal_x = None;
        }
        self.after_command(fonts);
        Action::None
    }

    fn type_text(&mut self, fonts: &mut Fonts, event: &KeyEvent, mods: ModifiersState) -> Action {
        // Ctrl+Alt is AltGr on Windows and Linux layouts, which does type characters.
        let shortcut = if MAC {
            mods.super_key() || mods.control_key()
        } else {
            mods.control_key() && !mods.alt_key()
        };
        let Some(text) = event.text.as_ref().filter(|t| !t.is_empty()) else {
            return Action::None;
        };
        if !shortcut && !text.chars().any(char::is_control) {
            self.editor.insert_text(text);
            self.goal_x = None;
            self.after_command(fonts);
        }
        Action::None
    }

    pub fn paste(&mut self, fonts: &mut Fonts, text: &str) {
        self.editor.paste(text);
        self.goal_x = None;
        self.after_command(fonts);
    }

    fn save(&mut self) {
        match self.editor.doc.save() {
            Ok(()) => {
                self.close_armed = None;
                self.notify("Saved");
            }
            Err(e) => self.notify(&format!("Save failed: {e}")),
        }
    }

    fn zoom(&mut self, fonts: &mut Fonts, delta: f32) {
        self.font_points = (self.font_points + delta).clamp(6.0, 48.0);
        self.rebuild_layout(fonts);
    }

    fn rebuild_layout(&mut self, fonts: &mut Fonts) {
        self.layout = TextLayout::new(fonts, self.font_points * self.window.scale_factor() as f32);
        self.window.request_redraw();
    }

    fn move_vertical(&mut self, fonts: &mut Fonts, lines: isize, extend: bool) {
        let x = match self.goal_x {
            Some(x) => x,
            None => self.caret_x(fonts),
        };
        let target = self.editor.caret_line() as isize + lines;
        let pos = if target < 0 {
            0
        } else if target as usize >= self.editor.doc.text.len_lines() {
            self.editor.len()
        } else {
            let line = target as usize;
            let (text, laid) = self.layout_line(fonts, line);
            let byte = byte_for_x(&laid, &text, x);
            self.editor.doc.text.line_to_char(line) + text[..byte].chars().count()
        };
        self.editor.move_to(pos, extend);
        self.goal_x = Some(x);
    }

    fn click(&mut self, fonts: &mut Fonts, extend: bool) {
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
        let pos = self.hit(fonts);
        match count {
            1 => self.editor.move_to(pos, extend),
            2 => self.editor.select_word_at(pos),
            _ => self.editor.select_line_at(pos),
        }
        self.dragging = count == 1;
        self.goal_x = None;
        self.after_command(fonts);
    }

    fn scroll_by(&mut self, dx: f32, dy: f32) {
        let max_y = (self.editor.doc.text.len_lines() - 1) as f32 * self.line_height();
        self.scroll_x = (self.scroll_x + dx).max(0.0);
        self.scroll_y = (self.scroll_y + dy).clamp(0.0, max_y);
        self.window.request_redraw();
    }

    /// Scrolls just enough to bring the caret into view.
    fn reveal(&mut self, fonts: &mut Fonts) {
        let line_height = self.line_height();
        let top = self.editor.caret_line() as f32 * line_height;
        let height = self.text_height();
        if top < self.scroll_y {
            self.scroll_y = top;
        } else if top + line_height > self.scroll_y + height {
            self.scroll_y = top + line_height - height;
        }
        let x = self.caret_x(fonts);
        let width = self.window.inner_size().width as f32 - self.text_left();
        let margin = self.layout.metrics.char_width * 4.0;
        if x < self.scroll_x {
            self.scroll_x = (x - width / 3.0).max(0.0);
        } else if x > self.scroll_x + width - margin {
            self.scroll_x = x - width + margin;
        }
    }

    /// Hands new edits to the highlighter and keeps the colour cache aligned with the text.
    fn after_command(&mut self, fonts: &mut Fonts) {
        let changes = std::mem::take(&mut self.editor.changes);
        if !changes.is_empty() {
            let version = self.editor.doc.version;
            self.styles.record(version, &changes);
            self.highlighter.edit(version, self.editor.doc.text.clone(), changes);
            self.close_armed = None;
        }
        self.reveal(fonts);
        self.update_title();
        self.window.request_redraw();
    }

    pub fn apply_highlights(&mut self, mut batch: Batch) {
        if let Some(name) = batch.syntax_name.take() {
            self.syntax_name = name;
        }
        self.styles.apply(batch);
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
        let char_width = self.layout.metrics.char_width;
        let gutter_width = self.gutter_width();
        let text_left = self.text_left();
        let status_height = self.status_height();
        let text_bottom = height - status_height;
        let p = self.palette;

        let line_count = self.editor.doc.text.len_lines();
        let first = (self.scroll_y / line_height) as usize;
        let last = (((self.scroll_y + text_bottom) / line_height).ceil() as usize + 1).min(line_count);
        let sel = self.editor.sel;
        let caret_line = self.editor.caret_line();

        // Shape visible lines first; painting below only borrows the results.
        let mut lines = Vec::with_capacity(last - first);
        let mut widest: f32 = 0.0;
        for line in first..last {
            let (text, laid) = self.layout_line(fonts, line);
            widest = widest.max(laid.w);
            lines.push((line, text, laid));
        }
        self.scroll_x = self.scroll_x.min((widest - (width - text_left) / 2.0).max(0.0));
        let x0 = text_left - self.scroll_x;

        if self.surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buffer) = self.surface.buffer_mut() else { return };
        let mut canvas = Canvas { pixels: &mut buffer, width: size.width as usize, height: size.height as usize };
        canvas.fill(0.0, 0.0, width, height, p.background);

        for (line, text, laid) in &lines {
            let line = *line;
            let top = line as f32 * line_height - self.scroll_y;
            if line == caret_line && sel.is_empty() {
                canvas.fill(0.0, top, width, top + line_height, p.line_highlight);
            }

            let line_start = self.editor.doc.text.line_to_char(line);
            let line_end = self.editor.line_end(line);
            if !sel.is_empty() && sel.start() <= line_end && sel.end() >= line_start {
                let a = sel.start().max(line_start) - line_start;
                let b = sel.end().min(line_end) - line_start;
                let xa = x0 + x_for_byte(laid, char_to_byte(text, a));
                let mut xb = x0 + x_for_byte(laid, char_to_byte(text, b));
                if sel.end() > line_end {
                    xb += char_width * 0.6; // the selected line break
                }
                canvas.fill(xa.max(text_left), top, xb.max(text_left), top + line_height, p.selection);
            }

            canvas.text(fonts, laid, x0, top + baseline, text_left, width);

            let color = if line == caret_line { p.gutter_active } else { p.gutter };
            let number = self.layout.line(fonts, &(line + 1).to_string(), &[], color, false);
            let nx = gutter_width - char_width * 1.5 - number.w;
            canvas.text(fonts, &number, nx, top + baseline, 0.0, gutter_width);

            if line == caret_line {
                let column = sel.head - line_start;
                let x = x0 + x_for_byte(laid, char_to_byte(text, column));
                if x >= text_left - 1.0 {
                    let caret_width = (self.window.scale_factor() as f32 * 2.0).round();
                    canvas.fill(x, top, x + caret_width, top + line_height, p.caret);
                }
            }
        }

        // Status bar.
        let top = text_bottom;
        canvas.fill(0.0, top, width, height, p.status_background);
        let line_start = self.editor.doc.text.line_to_char(caret_line);
        let mut left = format!("Line {}, Column {}", caret_line + 1, sel.head - line_start + 1);
        if let Some((message, at)) = &self.message {
            if at.elapsed() < MESSAGE_TIME {
                left.push_str("    ");
                left.push_str(message);
            }
        }
        let indent = match self.editor.indent {
            Indent::Tabs => "Tabs".to_string(),
            Indent::Spaces(n) => format!("Spaces: {n}"),
        };
        let right = format!("{indent}      {}", self.syntax_name);
        let text_y = top + ((status_height - line_height) / 2.0).round() + baseline;
        let pad = char_width * 1.5;
        let left = self.layout.line(fonts, &left, &[], p.status_foreground, true);
        canvas.text(fonts, &left, pad, text_y, 0.0, width);
        let right = self.layout.line(fonts, &right, &[], p.status_foreground, true);
        canvas.text(fonts, &right, width - pad - right.w, text_y, 0.0, width);

        if let Some(path) = std::env::var_os("OXIDE_SNAPSHOT") {
            let _ = crate::render::write_bmp(path.as_ref(), &buffer, size.width as usize, size.height as usize);
        }
        let _ = buffer.present();
        self.layout.end_frame();
    }
}

fn char_to_byte(text: &str, chars: usize) -> usize {
    text.char_indices().nth(chars).map_or(text.len(), |(i, _)| i)
}
