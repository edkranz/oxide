mod document;
mod editor;
mod find;
mod highlight;
mod input;
mod instance;
#[cfg(target_os = "macos")]
mod macos;
mod render;
mod session;
mod window;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use syntect::highlighting::{Theme, ThemeSet};
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
use crate::window::{Action, Palette, TabInit, Win, CONFIRM_WINDOW};
const THEME: &str = "base16-ocean.dark";
/// How long after a change the session is saved, so bursts of typing share one write.
const SESSION_DELAY: Duration = Duration::from_secs(2);

pub enum UserEvent {
    /// Colours for one tab: window, tab id, and the batch.
    Highlighted(WindowId, u64, Batch),
    /// Files from another `ox` invocation, Finder, or the Dock.
    Open(Vec<PathBuf>, Option<Waiter>),
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
    theme: Option<(Theme, Palette)>,
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
        let fonts = self.fonts.get_or_insert_with(|| {
            let fonts = Fonts::new();
            trace("fonts loaded");
            fonts
        });
        let (theme, palette) = self.theme.get_or_insert_with(|| {
            let mut themes = ThemeSet::load_defaults();
            let theme = themes.themes.remove(THEME).expect("bundled theme");
            let palette = Palette::from_theme(&theme);
            (theme, palette)
        });
        match Win::new(event_loop, fonts, tabs, active, theme.clone(), *palette, self.proxy.clone()) {
            Ok(win) => {
                let id = win.id();
                self.windows.insert(id, win);
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
                self.session_due = None;
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(due) => event_loop.set_control_flow(ControlFlow::WaitUntil(due)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        self.save_session_now();
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
        theme: None,
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
