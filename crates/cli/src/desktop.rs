//! Signals to the running desktop (`agentos-shell`).
//!
//! The desktop listens on `$XDG_RUNTIME_DIR/agentos-shell.sock` for one word
//! per connection: bring up the overlay to talk to the butler, open the
//! menu, or re-read the theme. A key binding runs `agentos-shell --summon`
//! (or `--menu`), and `agentos theme` says the theme changed.
//!
//! A signal carries no data, so nothing that arrives here can put words in
//! the user's mouth or approve anything; it only changes what is on screen.
//! The runtime directory is the user's own (mode 0700), and the socket is
//! made private too, so only this user's programs can send one.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The socket's name in the runtime directory.
pub const SOCKET: &str = "agentos-shell.sock";

/// What a signal asks of the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Bring up the overlay and type straight to the butler.
    Summon,
    /// Open the menu of everything the desktop can do.
    Menu,
    /// The theme file changed: draw with it.
    Theme,
}

impl Signal {
    pub fn word(self) -> &'static str {
        match self {
            Self::Summon => "summon",
            Self::Menu => "menu",
            Self::Theme => "theme",
        }
    }

    pub fn parse(line: &str) -> Option<Self> {
        match line.trim() {
            "summon" => Some(Self::Summon),
            "menu" => Some(Self::Menu),
            "theme" => Some(Self::Theme),
            _ => None,
        }
    }
}

/// The socket, given `XDG_RUNTIME_DIR`. Without an absolute runtime
/// directory there is no private place for it, so there is none.
pub fn socket_path_in(runtime_dir: Option<&str>) -> Option<PathBuf> {
    runtime_dir
        .filter(|dir| Path::new(dir).is_absolute())
        .map(|dir| PathBuf::from(dir).join(SOCKET))
}

/// This session's socket.
pub fn socket_path() -> Option<PathBuf> {
    socket_path_in(std::env::var("XDG_RUNTIME_DIR").ok().as_deref())
}

/// Send `signal` to the desktop listening at `path`.
pub fn send_to(path: &Path, signal: Signal) -> io::Result<()> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(format!("{}\n", signal.word()).as_bytes())
}

/// Send `signal` to this session's desktop. `NotFound` when there is no
/// runtime directory; a connection error when no desktop is running.
pub fn send(signal: Signal) -> io::Result<()> {
    let path = socket_path().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "没有 XDG_RUNTIME_DIR，找不到桌面")
    })?;
    send_to(&path, signal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;

    #[test]
    fn words_round_trip_and_nothing_else_is_a_signal() {
        for signal in [Signal::Summon, Signal::Menu, Signal::Theme] {
            assert_eq!(Signal::parse(signal.word()), Some(signal));
        }
        assert_eq!(Signal::parse("summon\n"), Some(Signal::Summon));
        // Guards "a signal carries no data": text is never a signal.
        for line in ["", "submit 清空笔记", "approve", "SUMMON now"] {
            assert_eq!(Signal::parse(line), None, "{line}");
        }
    }

    #[test]
    fn the_socket_lives_only_in_an_absolute_runtime_directory() {
        assert_eq!(
            socket_path_in(Some("/run/user/1000")),
            Some(PathBuf::from("/run/user/1000/agentos-shell.sock"))
        );
        assert_eq!(socket_path_in(Some("tmp")), None);
        assert_eq!(socket_path_in(None), None);
    }

    #[test]
    fn a_signal_arrives_as_one_line() {
        let dir = std::env::temp_dir().join(format!("agentos-desktop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SOCKET);
        let listener = UnixListener::bind(&path).unwrap();
        send_to(&path, Signal::Menu).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        io::BufReader::new(stream).read_line(&mut line).unwrap();
        assert_eq!(Signal::parse(&line), Some(Signal::Menu));
        let _ = std::fs::remove_dir_all(dir);
    }
}
