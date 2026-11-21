//! Sandboxes for code AgentOS runs but did not write: [plugins](crate::plugin)
//! (code the system generated for itself) and [MCP servers](crate::mcp)
//! (programs the user installed). Either may be buggy or hostile, and both
//! would otherwise run as the desktop user: free to read the user's files, to
//! rewrite AgentOS's own policy files, or to connect to the control socket and
//! approve the very steps they provoked. bubblewrap gives each such process
//! namespaces of its own and a read-only view of the system; each kind then
//! adds only what it needs.

use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// How sandboxed processes are isolated on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// bubblewrap: new namespaces, read-only system, no home, optional network.
    Bubblewrap,
    /// No isolation beyond a cleared environment and a timeout.
    None,
}

impl SandboxMode {
    /// `AGENTOS_SANDBOX=none` forces no sandbox; otherwise use bubblewrap if it
    /// actually works here (it can be installed yet blocked by the kernel).
    pub fn detect() -> Self {
        if std::env::var("AGENTOS_SANDBOX").as_deref() == Ok("none") {
            return Self::None;
        }
        let ok = std::process::Command::new("bwrap")
            .args([
                "--ro-bind",
                "/",
                "/",
                "--unshare-all",
                "--die-with-parent",
                "true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            Self::Bubblewrap
        } else {
            Self::None
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Bubblewrap => "bubblewrap 沙箱",
            Self::None => "无沙箱",
        }
    }
}

/// Directories of programs and libraries, readable in every sandbox.
pub const SYSTEM_DIRS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/lib32"];

/// Only what DNS, TLS, time zones and interpreter alternatives need from /etc.
const SYSTEM_FILES: &[&str] = &[
    "/etc/alternatives",
    "/etc/ssl",
    "/etc/ca-certificates",
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/localtime",
    "/etc/ld.so.cache",
];

/// A bubblewrap command whose process gets namespaces of its own (sharing
/// only the network, and only when `net`), dies with its parent, cannot
/// reach the controlling terminal, sees [`SYSTEM_DIRS`] and a few files under
/// /etc read-only, and has private `/proc`, `/dev` and `/tmp`. Nothing else of
/// the host is there: callers add what their kind of process needs, then the
/// environment and the program.
pub fn bwrap_base(net: bool) -> Command {
    let mut c = Command::new("bwrap");
    c.args(["--die-with-parent", "--new-session", "--unshare-all"]);
    if net {
        c.arg("--share-net");
    }
    for path in SYSTEM_DIRS.iter().chain(SYSTEM_FILES) {
        c.args(["--ro-bind-try", path, path]);
    }
    c.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
    c
}
