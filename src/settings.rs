//! User settings from `settings.toml` in the config folder. Missing keys use defaults.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use syntect::highlighting::{Theme, ThemeSet};

pub const DEFAULT_THEME: &str = "base16-ocean.dark";

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Editor font; `None` picks a platform monospace font.
    pub font_family: Option<String>,
    pub font_size: f32,
    pub theme: String,
    /// Indent width when a file gives no clue, and the width of a tab character.
    pub tab_size: usize,
    pub translate_tabs_to_spaces: bool,
    /// Guess tabs vs. spaces from each file's existing indentation.
    pub detect_indentation: bool,
    /// Run the file's formatter before every save.
    pub format_on_save: bool,
    /// Complete brackets and quotes while typing.
    pub auto_pairs: bool,
    pub caret_blink: bool,
    #[serde(rename = "formatter")]
    pub formatters: Vec<Formatter>,
}

/// A command that reads the buffer on stdin and writes the formatted text to stdout.
#[derive(Clone, Debug, Deserialize)]
pub struct Formatter {
    /// File extensions (without the dot) or exact file names, such as `Dockerfile`.
    pub extensions: Vec<String>,
    /// The program and its arguments. `$FILE` is replaced with the file's path.
    pub command: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size: 14.0,
            theme: DEFAULT_THEME.into(),
            tab_size: 4,
            translate_tabs_to_spaces: true,
            detect_indentation: true,
            format_on_save: false,
            auto_pairs: true,
            caret_blink: true,
            formatters: Vec::new(),
        }
    }
}

impl Settings {
    /// The formatter for `path`, matched by file name, then extension.
    pub fn formatter_for(&self, path: &Path) -> Option<&Formatter> {
        let name = path.file_name()?.to_str()?;
        let ext = path.extension().and_then(|e| e.to_str());
        self.formatters
            .iter()
            .find(|f| f.extensions.iter().any(|e| e == name || Some(e.trim_start_matches('.')) == ext))
    }
}

pub fn path() -> Option<PathBuf> {
    crate::config_dir().map(|d| d.join("settings.toml"))
}

/// Reads the settings file. A missing file means defaults; a broken one returns its
/// error alongside the defaults so the editor still starts.
pub fn load() -> (Settings, Option<String>) {
    let Some(path) = path() else { return (Settings::default(), None) };
    match std::fs::read_to_string(&path) {
        Ok(text) => match parse(&text) {
            Ok(settings) => (settings, None),
            Err(e) => (Settings::default(), Some(e)),
        },
        Err(_) => (Settings::default(), None),
    }
}

fn parse(text: &str) -> Result<Settings, String> {
    toml::from_str(text).map_err(|e| format!("settings.toml: {}", e.message()))
}

/// Creates the settings file from the commented template if it doesn't exist yet.
pub fn ensure_file() -> std::io::Result<PathBuf> {
    let path = path().ok_or_else(|| std::io::Error::other("no config folder"))?;
    if !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, TEMPLATE)?;
    }
    Ok(path)
}

/// A bundled theme by name, or `<config>/themes/<name>.tmTheme`.
pub fn load_theme(name: &str) -> Result<Theme, String> {
    let mut themes = ThemeSet::load_defaults();
    if let Some(theme) = themes.themes.remove(name) {
        return Ok(theme);
    }
    let file = crate::config_dir().map(|d| d.join("themes").join(format!("{name}.tmTheme")));
    match file {
        Some(file) if file.exists() => ThemeSet::get_theme(&file).map_err(|e| format!("theme {name}: {e}")),
        _ => {
            let mut names: Vec<_> = ThemeSet::load_defaults().themes.into_keys().collect();
            names.sort();
            Err(format!("unknown theme \"{name}\" (built in: {})", names.join(", ")))
        }
    }
}

const TEMPLATE: &str = r#"# oxide settings. Save this file to apply changes.

# font_family = "JetBrains Mono"
font_size = 14

# Built in: base16-ocean.dark, base16-eighties.dark, base16-mocha.dark, base16-ocean.light,
# InspiredGitHub, Solarized (dark), Solarized (light). Or put NAME.tmTheme in themes/.
theme = "base16-ocean.dark"

tab_size = 4
translate_tabs_to_spaces = true
detect_indentation = true

auto_pairs = true    # complete brackets and quotes as you type
caret_blink = true

# Format with Shift+Alt+F, or on every save:
format_on_save = false

# Formatters read the file on stdin and print the result. $FILE is the file's path.
# Uncomment the ones you have installed.

# [[formatter]]
# extensions = ["rs"]
# command = ["rustfmt", "--edition", "2021", "--emit", "stdout"]

# [[formatter]]
# extensions = ["js", "jsx", "ts", "tsx", "json", "css", "html", "md", "yaml", "yml"]
# command = ["prettier", "--stdin-filepath", "$FILE"]

# [[formatter]]
# extensions = ["py"]
# command = ["black", "--quiet", "-"]

# [[formatter]]
# extensions = ["go"]
# command = ["gofmt"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_and_matches_defaults() {
        let settings = parse(TEMPLATE).unwrap();
        assert_eq!(settings.theme, DEFAULT_THEME);
        assert_eq!(settings.font_size, 14.0);
        assert!(settings.formatters.is_empty());
    }

    #[test]
    fn formatters_match_by_extension_or_name() {
        let settings = parse(
            r#"
            [[formatter]]
            extensions = ["rs"]
            command = ["rustfmt"]
            [[formatter]]
            extensions = ["Dockerfile", ".toml"]
            command = ["taplo", "fmt", "-"]
            "#,
        )
        .unwrap();
        let program = |p: &str| settings.formatter_for(Path::new(p)).map(|f| f.command[0].as_str());
        assert_eq!(program("/a/main.rs"), Some("rustfmt"));
        assert_eq!(program("/a/Dockerfile"), Some("taplo"));
        assert_eq!(program("/a/Cargo.toml"), Some("taplo"));
        assert_eq!(program("/a/notes.txt"), None);
    }

    #[test]
    fn errors_name_the_problem() {
        let err = parse("font_size = \"big\"").unwrap_err();
        assert!(err.starts_with("settings.toml:"), "{err}");
    }

    #[test]
    fn every_built_in_theme_loads() {
        for name in ThemeSet::load_defaults().themes.keys() {
            assert!(load_theme(name).is_ok(), "{name}");
        }
        assert!(load_theme("no such theme").unwrap_err().contains("base16-ocean.dark"));
    }
}
