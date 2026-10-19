//! The macOS menu bar. Each item runs a `Command` in the focused window, and shows the
//! same shortcut the window would handle itself. AppKit gives shortcuts to the menu
//! first, so a key with a menu item arrives here rather than as a key press.

use crate::window::{Command, MenuState};
use muda::accelerator::Accelerator;
use muda::{
    AboutMetadata, CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use std::collections::HashMap;

pub struct MenuBar {
    _menu: Menu,
    commands: HashMap<MenuId, Command>,
    word_wrap: CheckMenuItem,
    case: CheckMenuItem,
    regex: CheckMenuItem,
    whole_word: CheckMenuItem,
    shown: Option<MenuState>,
}

impl MenuBar {
    /// Installs the menu bar. `on_select` gets the id of each item chosen; pass it to
    /// [`MenuBar::command`]. Call once the app has finished launching.
    pub fn install(on_select: impl Fn(MenuId) + Send + Sync + 'static) -> MenuBar {
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| on_select(event.id)));
        let mut commands = HashMap::new();
        let mut item = |text: &str, command: Command, keys: &str| {
            let id = MenuId::new(format!("{command:?}"));
            commands.insert(id.clone(), command);
            MenuItem::with_id(id, text, true, accelerator(keys))
        };
        let sep = PredefinedMenuItem::separator;

        let about = AboutMetadata {
            name: Some("oxide".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            copyright: Some("MIT License".into()),
            ..Default::default()
        };
        let app = submenu(
            "oxide",
            &[
                &PredefinedMenuItem::about(Some("About oxide"), Some(about)),
                &sep(),
                &item("Settings…", Command::Settings, "CmdOrCtrl+,"),
                &sep(),
                &PredefinedMenuItem::services(None),
                &sep(),
                &PredefinedMenuItem::hide(Some("Hide oxide")),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::show_all(None),
                &sep(),
                &item("Quit oxide", Command::Quit, "CmdOrCtrl+Q"),
            ],
        );
        let file = submenu(
            "File",
            &[
                &item("New Tab", Command::NewTab, "CmdOrCtrl+N"),
                &item("New Window", Command::NewWindow, "CmdOrCtrl+Shift+N"),
                &item("Open…", Command::Open, "CmdOrCtrl+O"),
                &sep(),
                &item("Save", Command::Save, "CmdOrCtrl+S"),
                &item("Save As…", Command::SaveAs, "CmdOrCtrl+Shift+S"),
                &item("Format", Command::Format, "Shift+Alt+F"),
                &sep(),
                &item("Close Tab", Command::CloseTab, "CmdOrCtrl+W"),
            ],
        );
        let edit = submenu(
            "Edit",
            &[
                &item("Undo", Command::Undo, "CmdOrCtrl+Z"),
                &item("Redo", Command::Redo, "CmdOrCtrl+Shift+Z"),
                &sep(),
                &item("Cut", Command::Cut, "CmdOrCtrl+X"),
                &item("Copy", Command::Copy, "CmdOrCtrl+C"),
                &item("Paste", Command::Paste, "CmdOrCtrl+V"),
                &item("Select All", Command::SelectAll, "CmdOrCtrl+A"),
                &sep(),
                &item("Toggle Comment", Command::ToggleComment, "CmdOrCtrl+/"),
            ],
        );
        let selection = submenu(
            "Selection",
            &[
                &item("Add Next Occurrence", Command::AddNextOccurrence, "CmdOrCtrl+D"),
                &item("Select All Occurrences", Command::SelectAllOccurrences, "Ctrl+CmdOrCtrl+G"),
                &item("Split into Lines", Command::SplitIntoLines, "CmdOrCtrl+Shift+L"),
                &sep(),
                &item("Add Caret Above", Command::AddCaretAbove, "Ctrl+Shift+Up"),
                &item("Add Caret Below", Command::AddCaretBelow, "Ctrl+Shift+Down"),
            ],
        );
        let toggle = |text: &str, command: Command, keys: &str| {
            let id = MenuId::new(format!("{command:?}"));
            (id.clone(), CheckMenuItem::with_id(id, text, true, false, accelerator(keys)))
        };
        let (case_id, case) = toggle("Match Case", Command::ToggleCase, "CmdOrCtrl+Alt+C");
        let (regex_id, regex) = toggle("Regular Expression", Command::ToggleRegex, "CmdOrCtrl+Alt+R");
        let (word_id, whole_word) = toggle("Whole Word", Command::ToggleWholeWord, "CmdOrCtrl+Alt+W");
        let (wrap_id, word_wrap) = toggle("Word Wrap", Command::WordWrap, "Alt+Z");
        let find = submenu(
            "Find",
            &[
                &item("Find…", Command::Find, "CmdOrCtrl+F"),
                &item("Find and Replace…", Command::FindReplace, "CmdOrCtrl+Alt+F"),
                &item("Find Next", Command::FindNext, "CmdOrCtrl+G"),
                &item("Find Previous", Command::FindPrevious, "CmdOrCtrl+Shift+G"),
                &sep(),
                &case,
                &regex,
                &whole_word,
                &sep(),
                &item("Go to Line…", Command::GoToLine, "Ctrl+G"),
            ],
        );
        let view = submenu(
            "View",
            &[
                &word_wrap,
                &sep(),
                &item("Zoom In", Command::ZoomIn, "CmdOrCtrl+="),
                &item("Zoom Out", Command::ZoomOut, "CmdOrCtrl+-"),
                &item("Actual Size", Command::ZoomReset, "CmdOrCtrl+0"),
                &sep(),
                &PredefinedMenuItem::fullscreen(None),
            ],
        );
        let window = submenu(
            "Window",
            &[
                &PredefinedMenuItem::minimize(None),
                &PredefinedMenuItem::maximize(Some("Zoom")),
                &sep(),
                &item("Show Next Tab", Command::NextTab, "CmdOrCtrl+Shift+]"),
                &item("Show Previous Tab", Command::PreviousTab, "CmdOrCtrl+Shift+["),
                &sep(),
                &PredefinedMenuItem::bring_all_to_front(None),
            ],
        );
        for (id, command) in [
            (case_id, Command::ToggleCase),
            (regex_id, Command::ToggleRegex),
            (word_id, Command::ToggleWholeWord),
            (wrap_id, Command::WordWrap),
        ] {
            commands.insert(id, command);
        }

        let menu = Menu::with_items(&[&app, &file, &edit, &selection, &find, &view, &window])
            .expect("build the menu bar");
        menu.init_for_nsapp();
        window.set_as_windows_menu_for_nsapp();
        MenuBar { _menu: menu, commands, word_wrap, case, regex, whole_word, shown: None }
    }

    /// The command for a chosen item, or `None` for items AppKit handles itself.
    pub fn command(&self, id: &MenuId) -> Option<Command> {
        self.commands.get(id).copied()
    }

    /// Ticks the toggles to match the focused window.
    pub fn show(&mut self, state: MenuState) {
        if self.shown == Some(state) {
            return;
        }
        self.shown = Some(state);
        self.word_wrap.set_checked(state.word_wrap);
        self.case.set_checked(state.find.case_sensitive);
        self.regex.set_checked(state.find.regex);
        self.whole_word.set_checked(state.find.whole_word);
    }
}

fn submenu(text: &str, items: &[&dyn IsMenuItem]) -> Submenu {
    Submenu::with_items(text, true, items).expect("build a menu")
}

fn accelerator(keys: &str) -> Option<Accelerator> {
    let parsed = keys.parse();
    debug_assert!(parsed.is_ok(), "bad accelerator {keys}");
    parsed.ok()
}
