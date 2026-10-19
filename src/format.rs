//! Formatter add-ons: pipe the buffer through an external command and apply the result.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `command` with `input` on stdin and returns its stdout. Errors are short,
/// user-facing messages.
pub fn run(command: &[String], input: &str, path: Option<&Path>) -> Result<String, String> {
    let (program, args) = command.split_first().ok_or("the formatter's command is empty")?;
    let file = path.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let mut child = Command::new(program)
        .args(args.iter().map(|a| a.replace("$FILE", &file)))
        .current_dir(path.and_then(Path::parent).unwrap_or(Path::new(".")))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("{program} is not installed or not on PATH"),
            _ => format!("could not run {program}: {e}"),
        })?;

    // Feed and drain the pipes on threads, so a large file can't fill a pipe and deadlock.
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_owned();
    std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let drain = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    };
    let stdout = drain(Box::new(child.stdout.take().unwrap()));
    let stderr = drain(Box::new(child.stderr.take().unwrap()));

    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err(format!("{program} took longer than {}s", TIMEOUT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = stdout.join().unwrap_or_default();
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr.join().unwrap_or_default()).into_owned();
        let detail = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        return Err(if detail.is_empty() {
            format!("{program} failed ({status})")
        } else {
            format!("{program}: {detail}")
        });
    }
    String::from_utf8(stdout).map_err(|_| format!("{program} printed text that isn't UTF-8"))
}

/// The smallest single edit turning `old` into `new`, as `(start, end, replacement)` with
/// char positions in `old`. `None` when they are equal.
pub fn minimal_edit(old: &str, new: &str) -> Option<(usize, usize, String)> {
    if old == new {
        return None;
    }
    let (mut prefix_bytes, mut prefix_chars) = (0, 0);
    for (a, b) in old.chars().zip(new.chars()) {
        if a != b {
            break;
        }
        prefix_bytes += a.len_utf8();
        prefix_chars += 1;
    }
    let (mut suffix_bytes, mut suffix_chars) = (0, 0);
    for (a, b) in old[prefix_bytes..].chars().rev().zip(new[prefix_bytes..].chars().rev()) {
        if a != b {
            break;
        }
        suffix_bytes += a.len_utf8();
        suffix_chars += 1;
    }
    let old_chars = old.chars().count();
    let replacement = new[prefix_bytes..new.len() - suffix_bytes].to_string();
    Some((prefix_chars, old_chars - suffix_chars, replacement))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(old: &str, (start, end, text): (usize, usize, String)) -> String {
        let chars: Vec<char> = old.chars().collect();
        chars[..start].iter().chain(text.chars().collect::<Vec<_>>().iter()).chain(&chars[end..]).collect()
    }

    #[test]
    fn minimal_edit_covers_only_the_change() {
        assert_eq!(minimal_edit("same", "same"), None);
        assert_eq!(minimal_edit("fn  main(){}", "fn main() {}"), Some((3, 10, "main() ".into())));
        for (old, new) in
            [("aaa", "aa"), ("ab", "abab"), ("héllo wörld", "héllo, wörld"), ("", "x"), ("x", "")]
        {
            let edit = minimal_edit(old, new).unwrap();
            assert_eq!(apply(old, edit), new, "{old:?} -> {new:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn runs_commands_and_reports_failures() {
        let cmd = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(run(&cmd(&["tr", "a-z", "A-Z"]), "shout\n", None).unwrap(), "SHOUT\n");
        let path = Path::new("/tmp/example.txt");
        assert_eq!(run(&cmd(&["echo", "$FILE"]), "", Some(path)).unwrap(), "/tmp/example.txt\n");
        let err = run(&cmd(&["sh", "-c", "echo 'line 3: bad' >&2; exit 1"]), "", None).unwrap_err();
        assert_eq!(err, "sh: line 3: bad");
        assert!(run(&cmd(&["oxide-no-such-formatter"]), "", None).unwrap_err().contains("not installed"));
    }
}
