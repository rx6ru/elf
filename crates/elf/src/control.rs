//! The control socket: one command line in, the reply out, then the connection closes.
//! The reply starts with `ok` or `err`; `history` adds one line per entry.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use crate::machine::Event;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Command {
    Start,
    Stop,
    Toggle,
    /// One key for tap and hold: press starts, or stops a dictation that a tap left open.
    Press,
    /// Release stops a hold; after a short press it keeps the dictation open (a tap).
    Release,
    Cancel,
    PasteAgain,
    Retry,
    History(usize),
    Status,
    Quit,
}

impl Command {
    pub fn parse(line: &str) -> Option<Command> {
        let mut words = line.split_whitespace();
        let cmd = match words.next()? {
            "start" => Command::Start,
            "stop" => Command::Stop,
            "toggle" => Command::Toggle,
            "press" => Command::Press,
            "release" => Command::Release,
            "cancel" => Command::Cancel,
            "paste-again" => Command::PasteAgain,
            "retry" => Command::Retry,
            "history" => Command::History(words.next().map_or(Some(10), |n| n.parse().ok())?),
            "status" => Command::Status,
            "quit" => Command::Quit,
            _ => return None,
        };
        words.next().is_none().then_some(cmd)
    }
}

#[derive(Debug)]
pub enum BindError {
    AlreadyRunning,
    Io(io::Error),
}

/// Binds the socket. A live socket means another `elfd` runs; a stale file is replaced.
pub fn bind(path: &Path) -> Result<UnixListener, BindError> {
    if UnixStream::connect(path).is_ok() {
        return Err(BindError::AlreadyRunning);
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).map_err(|e| {
        if e.kind() == io::ErrorKind::AddrInUse { BindError::AlreadyRunning } else { BindError::Io(e) }
    })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(BindError::Io)?;
    Ok(listener)
}

const IO_TIMEOUT: Duration = Duration::from_secs(2);
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Serves clients one at a time; each exchange is short.
pub fn serve(listener: UnixListener, events: Sender<Event>) {
    for conn in listener.incoming().flatten() {
        let _ = handle(conn, &events);
    }
}

fn handle(mut conn: UnixStream, events: &Sender<Event>) -> io::Result<()> {
    conn.set_read_timeout(Some(IO_TIMEOUT))?;
    conn.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut line = String::new();
    BufReader::new(&conn).take(256).read_line(&mut line)?;
    let reply = match Command::parse(&line) {
        None => "err unknown command".to_string(),
        Some(cmd) => {
            let (tx, rx) = mpsc::channel();
            if events.send(Event::Command(cmd, tx)).is_err() {
                "err daemon is stopping".to_string()
            } else {
                rx.recv_timeout(REPLY_TIMEOUT).unwrap_or_else(|_| "err no reply".into())
            }
        }
    };
    conn.write_all(reply.as_bytes())?;
    conn.write_all(b"\n")
}

/// The client side: sends one line and returns the whole reply.
pub fn request(socket: &Path, line: &str) -> io::Result<String> {
    let mut conn = UnixStream::connect(socket)?;
    conn.set_read_timeout(Some(REPLY_TIMEOUT + IO_TIMEOUT))?;
    conn.write_all(line.as_bytes())?;
    conn.write_all(b"\n")?;
    let mut reply = String::new();
    conn.read_to_string(&mut reply)?;
    Ok(reply.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_verb() {
        assert_eq!(Command::parse("start\n"), Some(Command::Start));
        assert_eq!(Command::parse("toggle"), Some(Command::Toggle));
        assert_eq!(Command::parse("paste-again"), Some(Command::PasteAgain));
        assert_eq!(Command::parse("press"), Some(Command::Press));
        assert_eq!(Command::parse("release"), Some(Command::Release));
        assert_eq!(Command::parse("history"), Some(Command::History(10)));
        assert_eq!(Command::parse("history 3"), Some(Command::History(3)));
        assert_eq!(Command::parse("quit"), Some(Command::Quit));
    }

    #[test]
    fn rejects_bad_lines() {
        assert_eq!(Command::parse(""), None);
        assert_eq!(Command::parse("dance"), None);
        assert_eq!(Command::parse("start now"), None);
        assert_eq!(Command::parse("history x"), None);
    }

    #[test]
    fn scenario_21_second_daemon_finds_the_live_socket() {
        let path = std::env::temp_dir().join(format!("elf-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let first = bind(&path).unwrap();
        assert!(matches!(bind(&path), Err(BindError::AlreadyRunning)));
        drop(first);
        // The file is left behind but nobody listens: it counts as stale.
        assert!(bind(&path).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn request_and_reply_round_trip() {
        let path = std::env::temp_dir().join(format!("elf-test-rt-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = bind(&path).unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || serve(listener, tx));
        std::thread::spawn(move || {
            if let Ok(Event::Command(cmd, reply)) = rx.recv() {
                let _ = reply.send(format!("ok got {cmd:?}"));
            }
        });
        assert_eq!(request(&path, "toggle").unwrap(), "ok got Toggle");
        assert_eq!(request(&path, "dance").unwrap(), "err unknown command");
        let _ = std::fs::remove_file(&path);
    }
}
