//! Hot exit: open windows, tabs, and unsaved text survive quitting and come back on launch.

use ropey::Rope;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Serialises writers, and records the newest snapshot written so a slow, older save
/// can never overwrite a newer one.
static WRITTEN: Mutex<u64> = Mutex::new(0);
static NEXT: AtomicU64 = AtomicU64::new(1);

#[derive(Default, Serialize, Deserialize)]
pub struct Session {
    pub windows: Vec<WindowState>,
}

#[derive(Serialize, Deserialize)]
pub struct WindowState {
    pub tabs: Vec<TabState>,
    pub active: usize,
}

#[derive(Serialize, Deserialize)]
pub struct TabState {
    pub path: Option<PathBuf>,
    /// The unsaved contents, if the tab differs from its file (or has none).
    pub text: Option<String>,
    pub anchor: usize,
    pub head: usize,
    pub scroll: (f32, f32),
}

/// A tab captured on the UI thread. Ropes clone in O(1), so taking a snapshot is cheap;
/// turning it into text happens on the writer thread.
pub struct TabSnapshot {
    pub path: Option<PathBuf>,
    pub unsaved: Option<Rope>,
    pub anchor: usize,
    pub head: usize,
    pub scroll: (f32, f32),
}

pub struct WindowSnapshot {
    pub tabs: Vec<TabSnapshot>,
    pub active: usize,
}

/// Where the session lives: `OXIDE_DATA_DIR`, or the platform's app-data folder.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("OXIDE_DATA_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        home().map(|h| h.join("Library/Application Support"))
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|h| h.join(".local/share")))
    };
    base.map(|b| b.join("oxide"))
}

fn session_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("session.json"))
}

pub fn load() -> Session {
    session_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn to_session(windows: Vec<WindowSnapshot>) -> Session {
    let windows = windows
        .into_iter()
        .map(|w| WindowState {
            active: w.active,
            tabs: w
                .tabs
                .into_iter()
                .map(|t| TabState {
                    path: t.path,
                    text: t.unsaved.map(|r| r.to_string()),
                    anchor: t.anchor,
                    head: t.head,
                    scroll: t.scroll,
                })
                .collect(),
        })
        .collect();
    Session { windows }
}

/// Writes the session now (used when quitting).
pub fn save(windows: Vec<WindowSnapshot>) -> io::Result<()> {
    write(NEXT.fetch_add(1, Ordering::Relaxed), windows)
}

/// Writes the session atomically, so a crash mid-write keeps the previous one.
fn write(seq: u64, windows: Vec<WindowSnapshot>) -> io::Result<()> {
    let mut written = WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if seq < *written {
        return Ok(());
    }
    let path = session_path().ok_or_else(|| io::Error::other("no data directory"))?;
    fs::create_dir_all(path.parent().unwrap())?;
    let json = serde_json::to_string(&to_session(windows))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &path)?;
    *written = seq;
    Ok(())
}

/// Saves on a background thread so typing never waits for the disk.
pub fn save_in_background(windows: Vec<WindowSnapshot>) {
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    std::thread::spawn(move || {
        if let Err(e) = write(seq, windows) {
            eprintln!("oxide: could not save session: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_round_trip_through_json() {
        let windows = vec![WindowSnapshot {
            active: 1,
            tabs: vec![
                TabSnapshot {
                    path: Some("/x/a.rs".into()),
                    unsaved: None,
                    anchor: 1,
                    head: 2,
                    scroll: (0.0, 40.0),
                },
                TabSnapshot {
                    path: None,
                    unsaved: Some(Rope::from_str("draft\n")),
                    anchor: 0,
                    head: 0,
                    scroll: (0.0, 0.0),
                },
            ],
        }];
        let json = serde_json::to_string(&to_session(windows)).unwrap();
        let session: Session = serde_json::from_str(&json).unwrap();
        let tabs = &session.windows[0].tabs;
        assert_eq!(session.windows[0].active, 1);
        assert_eq!(tabs[0].path.as_deref(), Some(std::path::Path::new("/x/a.rs")));
        assert_eq!(tabs[0].text, None);
        assert_eq!(tabs[1].text.as_deref(), Some("draft\n"));
        assert_eq!(tabs[0].scroll, (0.0, 40.0));
    }
}
