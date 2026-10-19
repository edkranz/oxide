// Release builds on Windows are GUI apps, so launching one doesn't open a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod document;
mod editor;
mod find;
mod format;
mod highlight;
mod input;
mod instance;
#[cfg(target_os = "macos")]
mod macos;
mod render;
mod session;
mod settings;
mod window;

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};
use syntect::highlighting::Theme;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::WindowId;

use crate::document::{Document, Selection};
use crate::highlight::Batch;
use crate::instance::Waiter;
use crate::render::Fonts;
use crate::session::TabState;
use crate::settings::Settings;
use crate::window::{Action, Palette, TabInit, Win, CONFIRM_WINDOW};
/// How long after a change the session is saved, so bursts of typing share one write.
const SESSION_DELAY: Duration = Duration::from_secs(2);

pub enum UserEvent {
    /// Colours for one tab: window, tab id, and the batch.
    Highlighted(WindowId, u64, Batch),
    /// Files from another `ox` invocation, Finder, or the Dock.
    Open(Vec<PathBuf>, Option<Waiter>),
    /// The full font database, scanned in the background after a fast start.
    FontsReady(cosmic_text::fontdb::Database),
    /// A formatter finished for a tab that had text `version`.
    Formatted { window: WindowId, tab: u64, version: u64, result: Result<String, String>, then_save: bool },
}
/// This is a comment. And yeah in oxide it is super fast and nice ! Let's gooooo
/// `~/.config/oxide` (or `%APPDATA%\oxide`): where add-ons such as syntaxes live.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("oxide"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|d| d.join("oxide"))
    }
}

pub fn trace(what: &str) {
    if std::env::var_os("OXIDE_TRACE").is_some() {
        eprintln!("oxide: {:>7.1?} {what}", start_time().elapsed());
    }
}

fn start_time() -> Instant {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *START.get_or_init(Instant::now)
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    paths: Vec<PathBuf>,
    /// Whether this process owns the instance socket. Only the primary instance keeps a
    /// session, so a second copy (say, a dev build) never overwrites it.
    primary: bool,
    fonts: Option<Fonts>,
    settings: Rc<Settings>,
    theme: Option<(Theme, Palette)>,
    /// A problem with the settings file, shown in the next window to open.
    settings_error: Option<String>,
    windows: HashMap<WindowId, Win>,
    last_focused: Option<WindowId>,
    clipboard: Option<arboard::Clipboard>,
    modifiers: ModifiersState,
    quit_armed: Option<Instant>,
    session_due: Option<Instant>,
}

impl App {
    /// Opens a window with the given tabs (or an empty one). Returns its id.
    fn open_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        tabs: Vec<TabInit>,
        active: usize,
    ) -> Option<WindowId> {
        if self.fonts.is_none() {
            let mut fonts = Fonts::from_cache().unwrap_or_else(Fonts::new);
            trace(if fonts.is_complete() { "fonts loaded (full scan)" } else { "fonts loaded (cached)" });
            if !fonts.is_complete() {
                let proxy = self.proxy.clone();
                Fonts::scan(move |db| {
                    let _ = proxy.send_event(UserEvent::FontsReady(db));
                });
            }
            if let Err(e) = fonts.set_family(self.settings.font_family.as_deref()) {
                self.settings_error.get_or_insert(e);
            }
            self.fonts = Some(fonts);
        }
        if self.theme.is_none() {
            let (theme, palette, error) = load_theme(&self.settings);
            self.theme = Some((theme, palette));
            if let Some(e) = error {
                self.settings_error.get_or_insert(e);
            }
        }
        let fonts = self.fonts.as_mut().unwrap();
        let (theme, palette) = self.theme.as_ref().unwrap();
        let settings = self.settings.clone();
        match Win::new(
            event_loop,
            fonts,
            tabs,
            active,
            settings,
            (theme.clone(), *palette),
            self.proxy.clone(),
        ) {
            Ok(mut win) => {
                if let Some(e) = self.settings_error.take() {
                    win.notify(&e);
                }
                let id = win.id();
                self.windows.insert(id, win);
                // The first frame has drawn, so the fonts it needed are known.
                self.fonts.as_mut().unwrap().save_cache();
                self.last_focused = Some(id);
                Some(id)
            }
            Err(e) => {
                eprintln!("oxide: could not open a window: {e}");
                None
            }
        }
    }

    /// Opens files in the most recently used window, bringing it to the front.
    fn open_files(&mut self, event_loop: &ActiveEventLoop, paths: Vec<PathBuf>, waiter: Option<Waiter>) {
        let target = self.last_focused.filter(|id| self.windows.contains_key(id));
        let target = target.or_else(|| self.windows.keys().next().copied());
        let (id, tabs) = match target {
            Some(id) => (id, self.windows.get_mut(&id).unwrap().open_paths(paths)),
            None => {
                let tabs = paths.into_iter().filter_map(open_document).map(TabInit::from).collect();
                let Some(id) = self.open_window(event_loop, tabs, 0) else { return };
                (id, self.windows[&id].tab_ids())
            }
        };
        let win = self.windows.get_mut(&id).unwrap();
        if let Some(waiter) = waiter {
            win.wait_for(tabs, waiter);
        }
        win.present();
    }

    fn restore_session(&mut self, event_loop: &ActiveEventLoop) {
        for window in session::load().windows {
            let tabs: Vec<TabInit> = window.tabs.into_iter().filter_map(restore_tab).collect();
            if !tabs.is_empty() {
                let active = window.active.min(tabs.len() - 1);
                self.open_window(event_loop, tabs, active);
            }
        }
    }

    /// Re-reads settings.toml and applies it to every window.
    fn reload_settings(&mut self) {
        let (settings, mut error) = settings::load();
        let settings = Rc::new(settings);
        let fonts = self.fonts.as_mut().expect("fonts load before the first window");
        if let Err(e) = fonts.set_family(settings.font_family.as_deref()) {
            error.get_or_insert(e);
        }
        let (theme, palette, theme_error) = load_theme(&settings);
        error = error.or(theme_error);
        for win in self.windows.values_mut() {
            win.apply_settings(fonts, settings.clone(), theme.clone(), palette);
            win.notify(error.as_deref().unwrap_or("Settings applied"));
        }
        self.theme = Some((theme, palette));
        self.settings = settings;
    }

    fn save_session_now(&self) {
        if self.primary {
            if let Err(e) = session::save(self.windows.values().map(Win::snapshot).collect()) {
                eprintln!("oxide: could not save session: {e}");
            }
        }
    }

    fn clipboard(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard.as_mut()
    }

    fn quit(&mut self, event_loop: &ActiveEventLoop) {
        // With a session, unsaved work is kept rather than discarded, so just go.
        if self.primary {
            self.save_session_now();
            event_loop.exit();
            return;
        }
        let dirty: Vec<WindowId> =
            self.windows.iter().filter(|(_, w)| w.is_dirty()).map(|(id, _)| *id).collect();
        let armed = self.quit_armed.is_some_and(|t| t.elapsed() < CONFIRM_WINDOW);
        if dirty.is_empty() || armed {
            event_loop.exit();
            return;
        }
        self.quit_armed = Some(Instant::now());
        for id in dirty {
            self.windows.get_mut(&id).unwrap().notify("Unsaved changes. Quit again to discard them.");
        }
    }
}

/// The theme named in `settings`, falling back to the default with an error message.
fn load_theme(settings: &Settings) -> (Theme, Palette, Option<String>) {
    let (theme, error) = match settings::load_theme(&settings.theme) {
        Ok(theme) => (theme, None),
        Err(e) => (settings::load_theme(settings::DEFAULT_THEME).expect("bundled theme"), Some(e)),
    };
    let palette = Palette::from_theme(&theme);
    (theme, palette, error)
}

/// Opens `path`, reporting failures on stderr.
fn open_document(path: PathBuf) -> Option<Document> {
    Document::open(path.clone()).map_err(|e| eprintln!("oxide: {}: {e}", path.display())).ok()
}

/// Rebuilds a tab from the session, skipping files that have since been deleted.
fn restore_tab(tab: TabState) -> Option<TabInit> {
    let doc = match (tab.text, tab.path) {
        (Some(text), path) => Document::with_unsaved(path, text),
        (None, Some(path)) if path.exists() => open_document(path)?,
        _ => return None,
    };
    let len = doc.text.len_chars();
    let sel = Selection { anchor: tab.anchor.min(len), head: tab.head.min(len) };
    Some(TabInit { doc, sel, scroll: tab.scroll })
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.windows.is_empty() {
            return;
        }
        if self.primary {
            self.restore_session(event_loop);
        }
        let paths = std::mem::take(&mut self.paths);
        if !paths.is_empty() {
            self.open_files(event_loop, paths, None);
        }
        if self.windows.is_empty() {
            self.open_window(event_loop, Vec::new(), 0);
        }
        if self.windows.is_empty() {
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Highlighted(id, tab, batch) => {
                if let Some(win) = self.windows.get_mut(&id) {
                    win.apply_highlights(tab, batch);
                }
            }
            UserEvent::Open(paths, waiter) => self.open_files(event_loop, paths, waiter),
            UserEvent::FontsReady(db) => {
                let Some(fonts) = self.fonts.as_mut() else { return };
                let result = fonts.replace_database(db);
                for win in self.windows.values_mut() {
                    win.fonts_changed(fonts);
                    if let Err(e) = &result {
                        win.notify(e);
                    }
                }
            }
            UserEvent::Formatted { window, tab, version, result, then_save } => {
                if let (Some(win), Some(fonts)) = (self.windows.get_mut(&window), self.fonts.as_mut()) {
                    win.apply_format(fonts, tab, version, result, then_save);
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        match &event {
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
                return;
            }
            WindowEvent::Focused(true) => self.last_focused = Some(id),
            _ => {}
        }
        let Some(win) = self.windows.get_mut(&id) else { return };
        let fonts = self.fonts.as_mut().expect("fonts load before the first window");
        let action = match event {
            WindowEvent::CloseRequested => Action::Close,
            WindowEvent::RedrawRequested => {
                win.draw(fonts);
                Action::None
            }
            event => win.handle(fonts, event, self.modifiers),
        };
        match action {
            Action::None => {}
            Action::Close => {
                if win.confirm_close() {
                    self.windows.remove(&id);
                    self.session_due.get_or_insert_with(|| Instant::now() + SESSION_DELAY);
                    if self.windows.is_empty() {
                        event_loop.exit();
                    }
                }
            }
            Action::Quit => self.quit(event_loop),
            Action::Copy(text) => {
                if let Some(clipboard) = self.clipboard() {
                    let _ = clipboard.set_text(text);
                }
            }
            Action::Paste => {
                let text = self.clipboard().and_then(|c| c.get_text().ok());
                if let (Some(text), Some(win)) = (text, self.windows.get_mut(&id)) {
                    win.paste(self.fonts.as_mut().unwrap(), &text);
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let mut settings_saved = false;
        for win in self.windows.values_mut() {
            settings_saved |= win.take_settings_saved();
        }
        if settings_saved {
            self.reload_settings();
        }
        if !self.primary {
            return;
        }
        let mut changed = false;
        for win in self.windows.values_mut() {
            changed |= win.take_session_changed();
        }
        if changed {
            self.session_due.get_or_insert_with(|| Instant::now() + SESSION_DELAY);
        }
        match self.session_due {
            Some(due) if Instant::now() >= due => {
                session::save_in_background(self.windows.values().map(Win::snapshot).collect());
                if let Some(fonts) = self.fonts.as_mut() {
                    fonts.save_cache();
                }
                self.session_due = None;
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(due) => event_loop.set_control_flow(ControlFlow::WaitUntil(due)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        self.save_session_now();
        if let Some(fonts) = self.fonts.as_mut() {
            fonts.save_cache();
        }
    }
}

const USAGE: &str = "\
usage: ox [options] [file ...]

Opens files in the running oxide, starting it if needed.

options:
  -w, --wait     wait until the files are closed (for git's core.editor, etc.)
  -h, --help     show this help
  -V, --version  show the version";

fn main() {
    start_time();
    let mut wait = false;
    let mut paths = Vec::new();
    for arg in std::env::args_os().skip(1) {
        match arg.to_str() {
            Some("-w" | "--wait") => wait = true,
            Some("-h" | "--help") => {
                println!("{USAGE}");
                return;
            }
            Some("-V" | "--version") => {
                println!("oxide {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            Some(s) if s.starts_with('-') && s.len() > 1 => {
                eprintln!("oxide: unknown option {s}\n\n{USAGE}");
                std::process::exit(2);
            }
            _ => paths.push(std::path::absolute(&arg).unwrap_or_else(|_| PathBuf::from(&arg))),
        }
    }

    // As a command, hand the files to the running instance, or start one in the
    // background, so the terminal comes straight back.
    if std::env::var_os("OXIDE_FOREGROUND").is_none() {
        if instance::send(&paths, wait).is_ok() {
            return;
        }
        if wait {
            // Start an empty instance, then wait on it like any other client.
            if spawn_instance(&[]) && instance::send_with_retry(&paths, true, Duration::from_secs(10)).is_ok()
            {
                return;
            }
        } else if launched_from_terminal() && spawn_instance(&paths) {
            return;
        }
    }

    let (settings, settings_error) = settings::load();
    let mut builder = EventLoop::<UserEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        // Without the default menu, Cmd+Q reaches us and can check for unsaved work.
        use winit::platform::macos::EventLoopBuilderExtMacOS;
        builder.with_default_menu(false);
    }
    let event_loop = builder.build().expect("create event loop");
    let proxy = event_loop.create_proxy();
    let primary = {
        let proxy = proxy.clone();
        instance::listen(move |request| {
            let _ = proxy.send_event(UserEvent::Open(request.paths, request.waiter));
        })
    };
    #[cfg(target_os = "macos")]
    {
        let proxy = std::sync::Mutex::new(proxy.clone());
        macos::on_open_files(move |paths| {
            let _ = proxy.lock().unwrap().send_event(UserEvent::Open(paths, None));
        });
    }
    let mut app = App {
        proxy,
        paths,
        primary,
        fonts: None,
        settings: Rc::new(settings),
        theme: None,
        settings_error,
        windows: HashMap::new(),
        last_focused: None,
        clipboard: None,
        modifiers: ModifiersState::empty(),
        quit_armed: None,
        session_due: None,
    };
    event_loop.run_app(&mut app).expect("run event loop");
}

fn launched_from_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

/// Starts the editor as a background process with `paths`. Returns false on failure.
fn spawn_instance(paths: &[PathBuf]) -> bool {
    use std::process::{Command, Stdio};
    let Ok(exe) = std::env::current_exe() else { return false };
    let mut command = Command::new(exe);
    command
        .args(paths)
        .env("OXIDE_FOREGROUND", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        // Own process group, so closing the terminal doesn't take the editor with it.
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().is_ok()
}
