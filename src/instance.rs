//! Single instance: the first oxide process listens on a local socket, and later `ox`
//! invocations hand it their files instead of starting another copy.
//!
//! The protocol is one JSON line per connection. The server answers `ok` once the request
//! is queued, then, for `--wait`, `done` when the files' tabs have all closed.

use interprocess::local_socket::{
    prelude::*, GenericFilePath, GenericNamespaced, ListenerOptions, Name, Stream,
};
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize)]
struct Message {
    paths: Vec<PathBuf>,
    wait: bool,
}

/// A request from another `ox` invocation.
pub struct Request {
    pub paths: Vec<PathBuf>,
    pub waiter: Option<Waiter>,
}

/// The connection of a client running `ox --wait`. Dropping it (or calling `done`)
/// releases the client.
pub struct Waiter(Stream);

impl Waiter {
    pub fn done(mut self) {
        let _ = self.0.write_all(b"done\n");
    }
}

/// The socket for `tag`, private to the current user. The app uses the tag "oxide";
/// tests use others.
fn name(tag: &str) -> io::Result<Name<'static>> {
    if cfg!(target_os = "macos") {
        // A file under the home folder: Finder-launched apps may lack $USER or $TMPDIR, but
        // always have $HOME, and other accounts can't reach it.
        return socket_path(tag)?.to_fs_name::<GenericFilePath>();
    }
    format!("{tag}-{}.sock", user_id()).to_ns_name::<GenericNamespaced>()
}

fn socket_path(tag: &str) -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is not set"))?;
    let dir = PathBuf::from(home).join("Library/Caches/oxide");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{tag}.sock")))
}

/// Distinguishes users sharing a machine-wide socket namespace (Linux, Windows).
fn user_id() -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let home = std::env::var_os("HOME").and_then(|h| std::fs::metadata(h).ok());
        home.map(|m| m.uid().to_string()).unwrap_or_default()
    }
    #[cfg(not(unix))]
    {
        std::env::var("USERNAME").unwrap_or_default()
    }
}

/// Hands `paths` to a running instance. Fails if there is none.
pub fn send(paths: &[PathBuf], wait: bool) -> io::Result<()> {
    send_to("oxide", paths, wait)
}

fn send_to(tag: &str, paths: &[PathBuf], wait: bool) -> io::Result<()> {
    let stream = Stream::connect(name(tag)?)?;
    let mut line = serde_json::to_string(&Message { paths: paths.to_vec(), wait })?;
    line.push('\n');
    (&stream).write_all(line.as_bytes())?;
    let mut reader = BufReader::new(&stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    if reply.trim() != "ok" {
        return Err(io::Error::other("no reply from the running instance"));
    }
    if wait {
        // Blocks until the server says done or goes away.
        reply.clear();
        let _ = reader.read_line(&mut reply);
    }
    Ok(())
}

/// Like `send`, retrying while a just-launched instance starts listening.
pub fn send_with_retry(paths: &[PathBuf], wait: bool, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match send(paths, wait) {
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            result => return result,
        }
    }
}

/// Becomes the primary instance, calling `handle` for each request on a background
/// thread. Returns false if another live instance already owns the socket.
pub fn listen(handle: impl Fn(Request) + Send + 'static) -> bool {
    listen_on("oxide", handle)
}

fn listen_on(tag: &str, handle: impl Fn(Request) + Send + 'static) -> bool {
    let Ok(name) = name(tag) else { return false };
    let listener = match ListenerOptions::new().name(name.clone()).create_sync() {
        Ok(listener) => listener,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            if Stream::connect(name.clone()).is_ok() {
                return false;
            }
            // A socket file left behind by a crash: nobody is listening, so take it over.
            match ListenerOptions::new().name(name).try_overwrite(true).create_sync() {
                Ok(listener) => listener,
                Err(_) => return false,
            }
        }
        Err(_) => return false,
    };
    std::thread::Builder::new()
        .name("instance".into())
        .spawn(move || {
            for stream in listener.incoming().filter_map(Result::ok) {
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_err() {
                    continue;
                }
                let Ok(message) = serde_json::from_str::<Message>(&line) else { continue };
                if (&stream).write_all(b"ok\n").is_err() {
                    continue;
                }
                let waiter = message.wait.then(|| Waiter(stream));
                handle(Request { paths: message.paths, waiter });
            }
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn requests_reach_the_listener_and_waiters_block_until_done() {
        let tag = format!("oxide-test-{}", std::process::id());
        let (tx, rx) = mpsc::channel();
        assert!(listen_on(&tag, move |request| tx.send(request).unwrap()));
        // A second listener on the same name must defer to the live one.
        assert!(!listen_on(&tag, |_| {}));

        send_to(&tag, &[PathBuf::from("/tmp/a.txt")], false).unwrap();
        let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(request.paths, vec![PathBuf::from("/tmp/a.txt")]);
        assert!(request.waiter.is_none());

        let client = {
            let tag = tag.clone();
            std::thread::spawn(move || send_to(&tag, &[PathBuf::from("/tmp/b.txt")], true))
        };
        let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!client.is_finished(), "--wait returned before the tab closed");
        request.waiter.unwrap().done();
        client.join().unwrap().unwrap();
        if cfg!(target_os = "macos") {
            let _ = std::fs::remove_file(socket_path(&tag).unwrap());
        }
    }

    #[test]
    fn sending_without_an_instance_fails() {
        assert!(send_to("oxide-test-nobody-listens", &[], false).is_err());
    }
}
