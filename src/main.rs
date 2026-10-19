mod document;
mod editor;
mod find;
mod highlight;
mod input;
mod render;
mod window;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;
use syntect::highlighting::{Theme, ThemeSet};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::WindowId;

use crate::document::Document;
use crate::highlight::Batch;
use crate::render::Fonts;
use crate::window::{Action, Palette, Win, CONFIRM_WINDOW};
const THEME: &str = "base16-ocean.dark";

pub enum UserEvent {
    /// Colours for one tab: window, tab id, and the batch.
    Highlighted(WindowId, u64, Batch),
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
    fonts: Option<Fonts>,
    theme: Option<(Theme, Palette)>,
    windows: HashMap<WindowId, Win>,
    clipboard: Option<arboard::Clipboard>,
    modifiers: ModifiersState,
    quit_armed: Option<Instant>,
}

impl App {
    /// Opens one window with a tab per path (or an empty tab).
    fn open_window(&mut self, event_loop: &ActiveEventLoop, paths: Vec<PathBuf>) {
        let mut docs = Vec::new();
        for path in paths {
            match Document::open(path.clone()) {
                Ok(doc) => docs.push(doc),
                Err(e) => eprintln!("oxide: {}: {e}", path.display()),
            }
        }
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
        match Win::new(event_loop, fonts, docs, theme.clone(), *palette, self.proxy.clone()) {
            Ok(win) => {
                self.windows.insert(win.id(), win);
            }
            Err(e) => eprintln!("oxide: could not open a window: {e}"),
        }
    }

    fn clipboard(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard.as_mut()
    }

    fn quit(&mut self, event_loop: &ActiveEventLoop) {
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

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.windows.is_empty() {
            return;
        }
        let paths = std::mem::take(&mut self.paths);
        self.open_window(event_loop, paths);
        if self.windows.is_empty() {
            event_loop.exit();
        }
    }

    fn user_event(&mut self, _: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Highlighted(id, tab, batch) => {
                if let Some(win) = self.windows.get_mut(&id) {
                    win.apply_highlights(tab, batch);
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let WindowEvent::ModifiersChanged(m) = &event {
            self.modifiers = m.state();
            return;
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
}

const USAGE: &str = "\
usage: oxide [options] [file ...]

options:
  -w, --wait     stay in the foreground until the window closes (for git, etc.)
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
            _ => paths.push(PathBuf::from(arg)),
        }
    }

    // Like `subl`, give the terminal back straight away unless asked to wait.
    if !wait && std::env::var_os("OXIDE_FOREGROUND").is_none() && detach() {
        return;
    }

    let mut builder = EventLoop::<UserEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        // Without the default menu, Cmd+Q reaches us and can check for unsaved work.
        use winit::platform::macos::EventLoopBuilderExtMacOS;
        builder.with_default_menu(false);
    }
    let event_loop = builder.build().expect("create event loop");
    let mut app = App {
        proxy: event_loop.create_proxy(),
        paths,
        fonts: None,
        theme: None,
        windows: HashMap::new(),
        clipboard: None,
        modifiers: ModifiersState::empty(),
        quit_armed: None,
    };
    event_loop.run_app(&mut app).expect("run event loop");
}

/// Re-launches this process in the background. Returns false if that isn't possible
/// (no terminal attached, or the spawn failed), in which case we just run here.
fn detach() -> bool {
    use std::io::IsTerminal;
    use std::process::{Command, Stdio};
    if !std::io::stdout().is_terminal() {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else { return false };
    let mut command = Command::new(exe);
    command
        .args(std::env::args_os().skip(1))
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
