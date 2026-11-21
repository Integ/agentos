//! MCP servers as capabilities.
//!
//! Any [Model Context Protocol](https://modelcontextprotocol.io) server that
//! speaks stdio can lend its tools to the butler. Each tool becomes a
//! capability named `mcp.<server>.<tool>` and goes through the policy engine
//! like everything else.
//!
//! Servers are listed in `<home>/mcp.json`, in the same shape Claude Desktop
//! uses, plus AgentOS's own risk and sandbox settings:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "fs": {
//!       "command": "npx",
//!       "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home/agent/work"],
//!       "env": {},
//!       "risk": "L1",
//!       "tools": { "read_file": "L0", "list_directory": "L0" },
//!       "trusted": true,
//!       "sandbox": { "write": ["/home/agent/work"] }
//!     }
//!   }
//! }
//! ```
//!
//! Trust comes from this file, never from the server:
//! - A tool's risk is `tools[name]`, else the server's `risk`, else **L2**
//!   (ask every time). A server's `destructiveHint` can only raise it to L3;
//!   `readOnlyHint` never lowers it.
//! - Results are untrusted (they taint the task) unless `"trusted": true`.
//! - The server runs with a cleared environment (PATH, HOME, LANG and the
//!   `env` given here), so it does not inherit the planner's API keys.
//! - The server runs in a bubblewrap sandbox (see [`Sandbox`]): the system
//!   read-only, a private home of its own in place of the user's, the network
//!   unless switched off, and only the paths its `read` and `write` rules
//!   name. AgentOS's own files are never in it: an unsandboxed server runs as
//!   the desktop user, so it could rewrite the policy files, or connect to the
//!   control socket and approve the very step its output provoked. Where no
//!   bubblewrap works, a server starts only with `"sandbox": false`, the
//!   user's explicit word that it may run with all of their rights.

use std::collections::HashMap;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentos_core::{Capability, CapabilityError, CapabilityName, CapabilitySpec, RiskLevel};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

use crate::registry::CapabilityRegistry;
use crate::sandbox::{self, SandboxMode};

const PROTOCOL_VERSION: &str = "2025-06-18";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Longest tool name the Claude and OpenAI APIs accept.
const MAX_TOOL_NAME: usize = 64;
/// Longest line read from a server, so a broken one cannot exhaust memory.
const MAX_LINE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Default risk for this server's tools: L0..L3.
    pub risk: Option<String>,
    /// Per-tool risk overrides.
    #[serde(default)]
    pub tools: HashMap<String, String>,
    /// Results come from a source the user trusts (no taint).
    #[serde(default)]
    pub trusted: bool,
    #[serde(default)]
    pub disabled: bool,
    /// How the server is contained. Absent means sandboxed with no extra rules.
    #[serde(default, deserialize_with = "sandbox_setting")]
    pub sandbox: Sandbox,
}

/// How an MCP server is contained: `"sandbox"` in its entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sandbox {
    /// `"sandbox": false`: the server runs with all of the user's rights. Only
    /// ever the user's explicit choice; nothing infers it.
    Off,
    /// The default (absent, `true`, or an object of rules): bubblewrap.
    On(SandboxRules),
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::On(SandboxRules::default())
    }
}

/// What a sandboxed server may reach besides the system's programs and
/// libraries and a private home of its own.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxRules {
    /// Host paths it may read: absolute, or starting with `~/`.
    #[serde(default)]
    pub read: Vec<String>,
    /// Host paths it may read and write.
    #[serde(default)]
    pub write: Vec<String>,
    /// Whether it may open network connections. On unless switched off: most
    /// servers call a web API, and `npx` or `uvx` fetch the server itself.
    #[serde(default = "network_by_default")]
    pub network: bool,
}

fn network_by_default() -> bool {
    true
}

impl Default for SandboxRules {
    fn default() -> Self {
        Self {
            read: Vec::new(),
            write: Vec::new(),
            network: network_by_default(),
        }
    }
}

/// `"sandbox"` is `true`, `false` or an object of [`SandboxRules`]. A typo in
/// a rule is an error: a security setting must never be quietly ignored.
fn sandbox_setting<'de, D>(deserializer: D) -> Result<Sandbox, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    match Value::deserialize(deserializer)? {
        Value::Bool(true) => Ok(Sandbox::default()),
        Value::Bool(false) => Ok(Sandbox::Off),
        rules @ Value::Object(_) => serde_json::from_value(rules)
            .map(Sandbox::On)
            .map_err(|e| D::Error::custom(format!("sandbox：{e}"))),
        _ => Err(D::Error::custom("sandbox 只能是 true、false 或一个对象")),
    }
}

/// Programs a sandboxed server may also run: software installed outside the
/// distribution's own directories.
const EXTRA_SYSTEM_DIRS: &[&str] = &["/opt"];
/// Account names, for tools that look up the user (git, ssh). No secrets.
const ACCOUNT_FILES: &[&str] = &["/etc/passwd", "/etc/group"];
/// Where programs are looked for when the daemon has no PATH.
const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// What starting servers in their sandboxes needs to know about this machine.
#[derive(Debug, Clone)]
pub struct Containment {
    sandbox: SandboxMode,
    /// The user's home: where `~/` in a rule points, and where a server finds
    /// its private home in place of the real one.
    home: PathBuf,
    /// One private home per server, kept across restarts (`npx` and `uvx`
    /// cache the server there rather than fetching it on every start).
    homes: PathBuf,
    /// AgentOS's own directories, by their real paths: the state directory and
    /// the old agentosd data. Hidden even under a rule that includes them.
    private_dirs: Vec<PathBuf>,
    /// AgentOS's files kept elsewhere: a control socket or an audit log moved
    /// out of the state directory. No rule may include them.
    private_files: Vec<PathBuf>,
}

impl Containment {
    /// `state` is the state directory, which also keeps the servers' private
    /// homes. `elsewhere` lists AgentOS's files and directories outside it;
    /// one that does not exist has nothing to hide, except a socket not yet
    /// created, which counts by the directory it will be in.
    pub fn new(
        sandbox: SandboxMode,
        home: Option<PathBuf>,
        state: &Path,
        elsewhere: &[PathBuf],
    ) -> Self {
        // A home of `/` would put the private home over the whole system.
        let home = home
            .filter(|home| home.is_absolute() && home.parent().is_some())
            .unwrap_or_else(|| PathBuf::from("/home/agentos"));
        let mut private_dirs = Vec::new();
        let mut private_files = Vec::new();
        for path in std::iter::once(state).chain(elsewhere.iter().map(PathBuf::as_path)) {
            let Some(real) = real_path(path) else {
                continue;
            };
            let kind = if real.is_dir() {
                &mut private_dirs
            } else {
                &mut private_files
            };
            if !kind.contains(&real) {
                kind.push(real);
            }
        }
        // A file in one of those directories is hidden with it; refusing every
        // rule that includes it would refuse `~` along with the state directory.
        private_files.retain(|file| !private_dirs.iter().any(|dir| file.starts_with(dir)));
        Self {
            sandbox,
            home,
            homes: state.join("mcp"),
            private_dirs,
            private_files,
        }
    }

    /// One `read` or `write` rule as a mount, or why it cannot be one.
    fn mount(&self, field: &str, written: &str, writable: bool) -> Result<Mount, String> {
        let at = match written.strip_prefix('~') {
            Some("") => self.home.clone(),
            Some(rest) if rest.starts_with('/') => self.home.join(&rest[1..]),
            _ => PathBuf::from(written),
        };
        let refuse = |why: &str| format!("sandbox.{field} 里的 {written} {why}");
        if !at.is_absolute() || at.components().any(|part| part == Component::ParentDir) {
            return Err(refuse("必须是绝对路径或以 ~/ 开头，不能含 .."));
        }
        let real = std::fs::canonicalize(&at).map_err(|e| refuse(&format!("不能用：{e}")))?;
        if real.parent().is_none() {
            return Err(refuse("是整个根目录；要不受限制，就写 \"sandbox\": false"));
        }
        if self.private_dirs.iter().any(|dir| real.starts_with(dir)) {
            return Err(refuse("在 AgentOS 自己的目录里，不能交给 MCP server"));
        }
        if self
            .private_files
            .iter()
            .any(|file| file.starts_with(&real))
        {
            return Err(refuse(
                "包含 AgentOS 的控制 socket 或审计日志，不能交给 MCP server",
            ));
        }
        Ok(Mount { at, real, writable })
    }
}

/// `path` with every symlink resolved; for a file not yet created (a socket),
/// its resolved directory joined with its name.
fn real_path(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok().or_else(|| {
        let dir = std::fs::canonicalize(path.parent()?).ok()?;
        Some(dir.join(path.file_name()?))
    })
}

/// A rule's path inside the sandbox: where it appears (as written, `~/`
/// expanded) and where its contents really are.
struct Mount {
    at: PathBuf,
    real: PathBuf,
    writable: bool,
}

fn not_started(name: &str, why: impl std::fmt::Display) -> String {
    format!("MCP server {name} 没有启动：{why}")
}

/// The server as the user asked, with all of their rights, but not with the
/// daemon's secrets: a cleared environment plus the configured `env`.
fn unsandboxed(cfg: &ServerConfig) -> Command {
    let mut cmd = Command::new(&cfg.command);
    cmd.args(&cfg.args).env_clear();
    for k in ["PATH", "HOME", "LANG", "USER", "TMPDIR", "XDG_RUNTIME_DIR"] {
        if let Ok(v) = std::env::var(k) {
            cmd.env(k, v);
        }
    }
    cmd.envs(&cfg.env);
    cmd
}

/// The server inside bubblewrap: the base sandbox, `/opt`, a private home
/// mounted where the user's home would be, its rules, and nothing of
/// AgentOS's own. Every rule is checked before anything starts, so a mistake
/// in `mcp.json` is reported, never turned into a wider sandbox.
fn sandboxed(
    name: &str,
    cfg: &ServerConfig,
    rules: &SandboxRules,
    contain: &Containment,
) -> Result<Command, String> {
    if contain.sandbox != SandboxMode::Bubblewrap {
        return Err(not_started(
            name,
            "这台机器上没有可用的 bubblewrap 沙箱。装上 bubblewrap，或者在 mcp.json 里给它写 \"sandbox\": false，让它以你的全部权限运行",
        ));
    }
    let mut mounts = Vec::new();
    for (field, paths, writable) in [("read", &rules.read, false), ("write", &rules.write, true)] {
        for written in paths {
            let mount = contain
                .mount(field, written, writable)
                .map_err(|why| not_started(name, why))?;
            mounts.push(mount);
        }
    }
    // Parents before children, so a narrower rule inside a wider one holds.
    mounts.sort_by_key(|mount| mount.at.components().count());
    let search = std::env::var("PATH").unwrap_or_else(|_| DEFAULT_PATH.into());
    let visible: Vec<&Path> = sandbox::SYSTEM_DIRS
        .iter()
        .chain(EXTRA_SYSTEM_DIRS)
        .map(Path::new)
        .chain(mounts.iter().map(|mount| mount.at.as_path()))
        .collect();
    let program =
        find_program(&cfg.command, &search, &visible).map_err(|why| not_started(name, why))?;
    let home = contain.homes.join(match sanitize(name) {
        s if s.is_empty() => "server".to_string(),
        s => s,
    });
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&home)
        .map_err(|e| {
            not_started(
                name,
                format!("无法创建它的私有目录 {}：{e}", home.display()),
            )
        })?;

    let mut cmd = sandbox::bwrap_base(rules.network);
    for path in EXTRA_SYSTEM_DIRS.iter().chain(ACCOUNT_FILES) {
        cmd.args(["--ro-bind-try", path, path]);
    }
    cmd.arg("--bind").arg(&home).arg(&contain.home);
    for mount in &mounts {
        cmd.arg(if mount.writable {
            "--bind"
        } else {
            "--ro-bind"
        })
        .arg(&mount.real)
        .arg(&mount.at);
    }
    // A rule wide enough to include AgentOS's own directories must still not
    // show them: an empty directory goes over each, last, so it wins.
    let mut hidden: Vec<PathBuf> = Vec::new();
    for mount in &mounts {
        for dir in &contain.private_dirs {
            if let Ok(rest) = dir.strip_prefix(&mount.real) {
                let at = mount.at.join(rest);
                if !hidden.contains(&at) {
                    hidden.push(at);
                }
            }
        }
    }
    for at in &hidden {
        cmd.arg("--tmpfs").arg(at);
    }
    cmd.arg("--chdir")
        .arg(&contain.home)
        .arg("--")
        .arg(&program)
        .args(&cfg.args);
    // bubblewrap hands its own environment to the server. The configured
    // `env` often holds tokens, so it goes there and never onto the command
    // line, which other users on the machine can read.
    cmd.env_clear()
        .env("PATH", &search)
        .env("HOME", &contain.home)
        .env("TMPDIR", "/tmp")
        .env(
            "LANG",
            std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into()),
        );
    if let Ok(user) = std::env::var("USER") {
        cmd.env("USER", user);
    }
    cmd.envs(&cfg.env);
    Ok(cmd)
}

/// Where `command` is: itself when it is a path, else the first match on
/// `search` (the daemon's PATH) that the sandbox can see, as bubblewrap's own
/// lookup would find it. A program only the daemon can see (installed in the
/// user's home, as nvm and uv do) is named, so the user knows what to add to
/// `read`.
fn find_program(command: &str, search: &str, visible: &[&Path]) -> Result<PathBuf, String> {
    let candidates: Vec<PathBuf> = if command.contains('/') {
        let path = PathBuf::from(command);
        if !path.is_absolute() {
            return Err(format!(
                "命令 {command} 必须是绝对路径，或者是 PATH 里能找到的名字"
            ));
        }
        vec![path]
    } else {
        std::env::split_paths(search)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(command))
            .collect()
    };
    let found: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|path| is_executable(path))
        .collect();
    let seen = |path: &Path| visible.iter().any(|root| path.starts_with(root));
    if let Some(path) = found.iter().find(|path| seen(path)) {
        return Ok(path.clone());
    }
    match found.first() {
        Some(path) => Err(format!(
            "命令 {} 在沙箱里看不到。把它所在的目录加进 sandbox.read（例如 \"~/.nvm\"），或者写 \"sandbox\": false",
            path.display()
        )),
        None => Err(format!("找不到命令 {command}")),
    }
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpConfig {
    #[serde(rename = "mcpServers", alias = "servers", default)]
    pub servers: HashMap<String, ServerConfig>,
}

impl McpConfig {
    /// Read `path`; a missing file means no servers.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} 格式不对：{e}", path.display()),
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                servers: HashMap::new(),
            }),
            Err(e) => Err(e),
        }
    }
}

pub fn parse_risk(s: &str) -> Option<RiskLevel> {
    Some(match s.trim().to_ascii_uppercase().as_str() {
        "L0" | "READONLY" | "READ_ONLY" => RiskLevel::ReadOnly,
        "L1" | "REVERSIBLE" => RiskLevel::Reversible,
        "L2" | "EXTERNAL" => RiskLevel::External,
        "L3" | "IRREVERSIBLE" => RiskLevel::Irreversible,
        _ => return None,
    })
}

/// `[a-z0-9_]` only, no `__` (that is the tool-name separator), no edge `_`.
pub fn sanitize(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        let c = if c.is_ascii_alphanumeric() { c } else { '_' };
        if c == '_' && (out.is_empty() || out.ends_with('_')) {
            continue;
        }
        out.push(c);
    }
    out.trim_end_matches('_').to_string()
}

/// The capability name for `tool` on `server`, short enough to be a model
/// tool name (`mcp__server__tool` ≤ 64 characters).
pub fn capability_for(server: &str, tool: &str) -> Option<CapabilityName> {
    let s = sanitize(server);
    let mut t = sanitize(tool);
    if s.is_empty() || t.is_empty() {
        return None;
    }
    let budget = MAX_TOOL_NAME.checked_sub("mcp____".len() + s.len())?;
    if budget == 0 {
        return None;
    }
    t.truncate(budget);
    let t = t.trim_end_matches('_');
    Some(CapabilityName::new(format!("mcp.{s}.{t}")))
}

/// The risk the policy engine uses for a tool: the user's setting, raised
/// (never lowered) by a server that says the tool is destructive.
pub fn tool_risk(cfg: &ServerConfig, tool: &str, annotations: Option<&Value>) -> RiskLevel {
    let base = cfg
        .tools
        .get(tool)
        .or(cfg.risk.as_ref())
        .and_then(|r| parse_risk(r))
        .unwrap_or(RiskLevel::External);
    let destructive = annotations
        .and_then(|a| a.get("destructiveHint"))
        .and_then(Value::as_bool)
        == Some(true);
    if destructive {
        RiskLevel::Irreversible
    } else {
        base
    }
}

type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct ForgetPending<'a> {
    pending: &'a Pending,
    id: u64,
}

impl Drop for ForgetPending<'_> {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}

/// A running stdio MCP server.
pub struct McpClient {
    name: String,
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    _child: Mutex<Child>,
}

impl McpClient {
    /// Start the server (in its sandbox, unless the user switched it off),
    /// initialize the session and return the client.
    pub async fn start(
        name: &str,
        cfg: &ServerConfig,
        contain: &Containment,
    ) -> Result<Arc<Self>, String> {
        let mut cmd = match &cfg.sandbox {
            Sandbox::Off => unsandboxed(cfg),
            Sandbox::On(rules) => sandboxed(name, cfg, rules, contain)?,
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("无法启动 MCP server {name}（{}）：{e}", cfg.command))?;
        let stdin = child.stdin.take().ok_or("没有 stdin")?;
        let stdout = child.stdout.take().ok_or("没有 stdout")?;
        let pending: Pending = Arc::default();
        tokio::spawn(read_loop(name.to_string(), stdout, pending.clone()));
        let client = Arc::new(Self {
            name: name.to_string(),
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
            _child: Mutex::new(child),
        });
        tokio::time::timeout(STARTUP_TIMEOUT, async {
            client
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": {},
                        "clientInfo": {"name": "agentos", "version": env!("CARGO_PKG_VERSION")},
                    }),
                )
                .await?;
            client
                .send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .await
        })
        .await
        .map_err(|_| format!("MCP server {name} 启动超时"))??;
        Ok(client)
    }

    async fn send(&self, msg: &Value) -> Result<(), String> {
        let mut line = msg.to_string();
        line.push('\n');
        let mut w = self.stdin.lock().await;
        w.write_all(line.as_bytes())
            .await
            .map_err(|e| format!("MCP server {} 已退出：{e}", self.name))?;
        w.flush().await.map_err(|e| e.to_string())
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        // A call may be abandoned at any await (its task was cancelled): the
        // guard forgets the request however this function is left, so a late
        // or missing response never leaves an entry behind.
        let _forget = ForgetPending {
            pending: &self.pending,
            id,
        };
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(format!("MCP server {} 已退出", self.name)),
            Err(_) => Err(format!("MCP server {} 响应超时", self.name)),
        }
    }

    /// Every tool the server offers (following pagination).
    pub async fn list_tools(&self) -> Result<Vec<Value>, String> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..50 {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let r = self.request("tools/list", params).await?;
            if let Some(a) = r.get("tools").and_then(Value::as_array) {
                tools.extend(a.iter().cloned());
            }
            cursor = r
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    pub async fn call_tool(&self, tool: &str, args: Value) -> Result<Value, String> {
        self.request("tools/call", json!({"name": tool, "arguments": args}))
            .await
    }
}

async fn read_loop(name: String, stdout: tokio::process::ChildStdout, pending: Pending) {
    let mut r = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = match (&mut r)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut buf)
            .await
        {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        if buf.len() > MAX_LINE {
            eprintln!("MCP server {name} 输出了过长的一行，断开");
            break;
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
            continue; // stray logging on stdout
        };
        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
            continue; // notification or server→client request: not supported
        };
        if msg.get("method").is_some() {
            continue;
        }
        let tx = pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        if let Some(tx) = tx {
            let result = match msg.get("error") {
                Some(e) => Err(format!(
                    "MCP 错误：{}",
                    e.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("未知错误")
                )),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(result);
        }
    }
    // Fail everything still waiting.
    pending.lock().unwrap_or_else(|p| p.into_inner()).clear();
}

/// One MCP tool as a capability.
pub struct McpTool {
    spec: CapabilitySpec,
    tool: String,
    client: Arc<McpClient>,
    untrusted: bool,
}

/// Text for the human from a `tools/call` result.
fn result_text(r: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(items) = r.get("content").and_then(Value::as_array) {
        for c in items {
            match c.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = c.get("text").and_then(Value::as_str) {
                        parts.push(t.to_string());
                    }
                }
                Some(other) => parts.push(format!("[{other}]")),
                None => {}
            }
        }
    }
    if parts.is_empty() {
        if let Some(s) = r.get("structuredContent") {
            return s.to_string();
        }
    }
    let text = parts.join("\n");
    const MAX: usize = 16 * 1024;
    if text.len() > MAX {
        let mut cut = MAX;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…（已截断）", &text[..cut])
    } else {
        text
    }
}

#[async_trait]
impl Capability for McpTool {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        let args = if args.is_null() { json!({}) } else { args };
        let r = self
            .client
            .call_tool(&self.tool, args)
            .await
            .map_err(CapabilityError::Failed)?;
        let text = result_text(&r);
        if r.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(CapabilityError::Failed(text));
        }
        let mut out = json!({"message": text});
        if let Some(s) = r.get("structuredContent") {
            out["structured"] = s.clone();
        }
        Ok(out)
    }

    fn untrusted_output(&self) -> bool {
        self.untrusted
    }
}

/// Start every enabled server in `cfg` and register its tools. Returns one
/// line per server for the console (what was registered, or why not).
pub async fn register_all(
    reg: &mut CapabilityRegistry,
    cfg: &McpConfig,
    contain: &Containment,
) -> Vec<String> {
    let mut notes = Vec::new();
    let mut names: Vec<_> = cfg.servers.keys().cloned().collect();
    names.sort();
    for name in names {
        let sc = &cfg.servers[&name];
        if sc.disabled {
            continue;
        }
        let client = match McpClient::start(&name, sc, contain).await {
            Ok(c) => c,
            Err(e) => {
                notes.push(e);
                continue;
            }
        };
        let tools = match client.list_tools().await {
            Ok(t) => t,
            Err(e) => {
                notes.push(format!("MCP server {name} 列出工具失败：{e}"));
                continue;
            }
        };
        let mut added = Vec::new();
        for t in tools {
            let Some(tool) = t.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(cap) = capability_for(&name, tool) else {
                notes.push(format!("MCP 工具 {name}/{tool} 的名字无法使用，已跳过"));
                continue;
            };
            if reg.get(&cap).is_some() {
                notes.push(format!(
                    "MCP 工具 {name}/{tool} 与已有能力 {cap} 重名，已跳过"
                ));
                continue;
            }
            let description = t
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or(tool)
                .chars()
                .take(1024)
                .collect::<String>();
            let schema = t
                .get("inputSchema")
                .cloned()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            let risk = tool_risk(sc, tool, t.get("annotations"));
            reg.register(Arc::new(McpTool {
                spec: CapabilitySpec {
                    name: cap.clone(),
                    description: format!("（MCP · {name}）{description}"),
                    risk,
                    input_schema: schema,
                },
                tool: tool.to_string(),
                client: client.clone(),
                untrusted: !sc.trusted,
            }));
            added.push(cap.to_string());
        }
        let contained = match &sc.sandbox {
            Sandbox::On(rules) if rules.network => "在沙箱里",
            Sandbox::On(_) => "在沙箱里，不能联网",
            Sandbox::Off => "不在沙箱里，有你的全部权限",
        };
        let trust = if sc.trusted {
            ""
        } else {
            "；结果视为不可信"
        };
        notes.push(format!(
            "MCP server {name}：{} 个工具（{contained}{trust}）",
            added.len()
        ));
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal stdio MCP server in Python: two tools, one destructive,
    /// and an `env` tool that reports whether a secret leaked through.
    const FAKE_SERVER: &str = r#"
import json, os, sys
TOOLS = [
  {"name": "echo", "description": "Echo text back", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
  {"name": "wipe-all", "description": "Delete everything", "inputSchema": {"type": "object"}, "annotations": {"destructiveHint": True, "readOnlyHint": False}},
  {"name": "env", "description": "Show env", "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": True}},
  {"name": "fail", "description": "Always fails", "inputSchema": {"type": "object"}},
]
print("starting up", flush=True)  # stray log line on stdout must be ignored
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    m, p = msg["method"], msg.get("params", {})
    if m == "initialize":
        r = {"protocolVersion": p["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}
    elif m == "tools/list":
        if p.get("cursor") == "2":
            r = {"tools": TOOLS[2:]}
        else:
            r = {"tools": TOOLS[:2], "nextCursor": "2"}
    elif m == "tools/call":
        a = p.get("arguments", {})
        if p["name"] == "echo":
            r = {"content": [{"type": "text", "text": "echo: " + a.get("text", "")}]}
        elif p["name"] == "env":
            r = {"content": [{"type": "text", "text": os.environ.get("SECRET_KEY", "none") + "/" + os.environ.get("GIVEN", "none")}]}
        elif p["name"] == "fail":
            r = {"content": [{"type": "text", "text": "it broke"}], "isError": True}
        else:
            r = {"content": [{"type": "text", "text": "wiped"}]}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "no such method"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": r}), flush=True)
"#;

    fn fake_config(
        tag: &str,
        risk: Option<&str>,
        trusted: bool,
    ) -> (McpConfig, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "agentos-mcp-{tag}-{}-{}",
            std::process::id(),
            agentos_core::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("server.py");
        std::fs::write(&script, FAKE_SERVER).unwrap();
        let mut tools = HashMap::new();
        tools.insert("env".to_string(), "L0".to_string());
        let sc = ServerConfig {
            command: "python3".into(),
            args: vec!["-I".into(), script.display().to_string()],
            env: [("GIVEN".to_string(), "yes".to_string())].into(),
            risk: risk.map(str::to_string),
            tools,
            trusted,
            disabled: false,
            // These tests are about the protocol; the sandbox has its own.
            sandbox: Sandbox::Off,
        };
        (
            McpConfig {
                servers: [("Fake Srv".to_string(), sc)].into(),
            },
            dir,
        )
    }

    /// A machine with no working sandbox, for servers the user let out of it.
    fn no_sandbox() -> Containment {
        Containment::new(
            SandboxMode::None,
            None,
            &std::env::temp_dir().join("agentos-mcp-no-state"),
            &[],
        )
    }

    fn python3() -> bool {
        std::process::Command::new("python3")
            .arg("-V")
            .output()
            .is_ok()
    }

    /// A fake user's machine: a home with a secret in it, and AgentOS's state
    /// directory inside that home, as on a real machine. Paths are resolved,
    /// so what a sandbox shows can be compared with where things really are.
    struct Machine {
        root: PathBuf,
        home: PathBuf,
        state: PathBuf,
    }

    impl Machine {
        fn new(tag: &str) -> Self {
            let root = std::fs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!(
                    "agentos-mcp-box-{tag}-{}-{}",
                    std::process::id(),
                    agentos_core::now_ms()
                ));
            let home = root.join("home");
            let state = home.join(".local/state/agentos");
            std::fs::create_dir_all(&state).unwrap();
            std::fs::write(home.join("secret.txt"), "the user's secret").unwrap();
            std::fs::write(state.join("pin.json"), "{\"hash\":\"x\"}").unwrap();
            Self { root, home, state }
        }

        fn dir(&self, name: &str) -> PathBuf {
            let dir = self.root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn containment(&self, sandbox: SandboxMode) -> Containment {
            Containment::new(sandbox, Some(self.home.clone()), &self.state, &[])
        }
    }

    impl Drop for Machine {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn rules(read: &[&str], write: &[&str], network: bool) -> SandboxRules {
        SandboxRules {
            read: read.iter().map(|path| path.to_string()).collect(),
            write: write.iter().map(|path| path.to_string()).collect(),
            network,
        }
    }

    fn sandboxed_args(
        machine: &Machine,
        cfg: &ServerConfig,
        rules: &SandboxRules,
    ) -> Result<(Vec<String>, Command), String> {
        let contain = machine.containment(SandboxMode::Bubblewrap);
        let cmd = sandboxed("Fake Srv", cfg, rules, &contain)?;
        let args = cmd
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        Ok((args, cmd))
    }

    /// Where `flag from to` first appears in `args`.
    fn mount_at(args: &[String], flag: &str, from: &Path, to: &Path) -> Option<usize> {
        args.windows(3)
            .position(|w| w[0] == flag && Path::new(&w[1]) == from && Path::new(&w[2]) == to)
    }

    #[test]
    fn names_are_safe_tool_names() {
        assert_eq!(sanitize("Fake Srv"), "fake_srv");
        assert_eq!(sanitize("wipe-all"), "wipe_all");
        assert_eq!(sanitize("a__b"), "a_b");
        assert_eq!(sanitize("--x--"), "x");
        assert_eq!(
            capability_for("Fake Srv", "wipe-all").unwrap().as_str(),
            "mcp.fake_srv.wipe_all"
        );
        let long = capability_for("srv", &"x".repeat(200)).unwrap();
        let tool = agentos_planner::tool_name(&long);
        assert!(tool.len() <= MAX_TOOL_NAME, "{tool}");
        assert_eq!(agentos_planner::capability_name(&tool), long);
        assert!(capability_for("", "x").is_none());
        assert!(capability_for("srv", "!!!").is_none());
    }

    #[test]
    fn risk_comes_from_the_user_and_hints_only_raise_it() {
        let sc = ServerConfig {
            risk: Some("L1".into()),
            tools: [("read".to_string(), "L0".to_string())].into(),
            ..ServerConfig::default()
        };
        let ro = json!({"readOnlyHint": true});
        let destructive = json!({"destructiveHint": true});
        assert_eq!(tool_risk(&sc, "read", None), RiskLevel::ReadOnly);
        assert_eq!(tool_risk(&sc, "write", Some(&ro)), RiskLevel::Reversible);
        assert_eq!(
            tool_risk(&sc, "read", Some(&destructive)),
            RiskLevel::Irreversible
        );
        let none = ServerConfig {
            risk: None,
            tools: HashMap::new(),
            ..sc
        };
        assert_eq!(tool_risk(&none, "x", Some(&ro)), RiskLevel::External);
    }

    #[test]
    fn config_accepts_the_claude_desktop_shape() {
        let c: McpConfig = serde_json::from_value(json!({
            "mcpServers": {"fs": {"command": "npx", "args": ["-y", "x"]}}
        }))
        .unwrap();
        assert_eq!(c.servers["fs"].command, "npx");
        assert!(!c.servers["fs"].trusted);
        let missing = McpConfig::load(Path::new("/nonexistent/mcp.json")).unwrap();
        assert!(missing.servers.is_empty());
    }

    #[test]
    fn a_server_is_sandboxed_unless_the_user_says_otherwise() {
        let server = |sandbox: Value| {
            serde_json::from_value::<McpConfig>(json!({
                "mcpServers": {"s": {"command": "x", "sandbox": sandbox}}
            }))
            .map(|c| c.servers["s"].sandbox.clone())
        };
        let plain: McpConfig =
            serde_json::from_value(json!({"mcpServers": {"s": {"command": "x"}}})).unwrap();
        // Guards "outside code runs sandboxed": saying nothing is the
        // sandbox, with the network (most servers need it) and no paths.
        assert_eq!(plain.servers["s"].sandbox, Sandbox::default());
        assert_eq!(Sandbox::default(), Sandbox::On(rules(&[], &[], true)));
        assert_eq!(server(json!(true)).unwrap(), Sandbox::default());
        assert_eq!(server(json!(false)).unwrap(), Sandbox::Off);
        assert_eq!(
            server(json!({"read": ["/srv/docs"], "write": ["~/work"], "network": false})).unwrap(),
            Sandbox::On(rules(&["/srv/docs"], &["~/work"], false))
        );
        // A typo in a security setting is an error, never a quiet default.
        let typo = server(json!({"wirte": ["~/work"]}))
            .unwrap_err()
            .to_string();
        assert!(typo.contains("wirte"), "{typo}");
        assert!(server(json!("yes")).is_err());
        assert!(server(json!(null)).is_err());
    }

    #[test]
    fn the_sandbox_shows_a_server_its_private_home_and_its_rules_only() {
        let machine = Machine::new("rules");
        let docs = machine.dir("docs");
        let work = machine.home.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let cfg = ServerConfig {
            command: "sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
            env: [("API_TOKEN".to_string(), "tok-123".to_string())].into(),
            ..ServerConfig::default()
        };
        let rules = rules(&[docs.to_str().unwrap()], &["~/work"], false);
        let (args, cmd) = sandboxed_args(&machine, &cfg, &rules).unwrap();
        let private = machine.state.join("mcp/fake_srv");
        // Guards the private home: in place of the user's home the server
        // finds a directory of its own, kept in the state directory and
        // readable by nobody else.
        assert!(mount_at(&args, "--bind", &private, &machine.home).is_some());
        assert_eq!(
            std::fs::metadata(&private).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Its rules, read-only and writable as written.
        assert!(mount_at(&args, "--ro-bind", &docs, &docs).is_some());
        assert!(mount_at(&args, "--bind", &work, &work).is_some());
        // Nothing of AgentOS's own state is mounted anywhere.
        assert!(!args.windows(3).any(|w| {
            (w[0] == "--bind" || w[0] == "--ro-bind")
                && Path::new(&w[2]).starts_with(&machine.state)
        }));
        assert!(!args.iter().any(|arg| arg == "--share-net"));
        // The program runs by its full path, with the server's arguments.
        let end = args.iter().position(|arg| arg == "--").unwrap();
        assert!(Path::new(&args[end + 1]).is_absolute(), "{}", args[end + 1]);
        assert!(args[end + 1].ends_with("/sh"));
        assert_eq!(args[end + 2..], ["-c", "exit 0"]);
        // Tokens in `env` reach the server through the environment, never
        // the command line, which other users of the machine can read.
        assert!(!args.iter().any(|arg| arg.contains("tok-123")));
        let envs: HashMap<String, String> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert_eq!(envs["API_TOKEN"], "tok-123");
        assert_eq!(Path::new(&envs["HOME"]), machine.home);
        assert_eq!(envs["TMPDIR"], "/tmp");
        assert!(!envs.contains_key("XDG_RUNTIME_DIR"));

        // The network is there unless switched off.
        let open = SandboxRules {
            network: true,
            ..rules
        };
        let (args, _) = sandboxed_args(&machine, &cfg, &open).unwrap();
        assert!(args.iter().any(|arg| arg == "--share-net"));
    }

    #[test]
    fn a_wide_rule_never_uncovers_agentos_own_files() {
        let machine = Machine::new("wide");
        let cfg = ServerConfig {
            command: "sh".into(),
            ..ServerConfig::default()
        };
        // The whole home may be given; AgentOS's state in it stays hidden
        // under an empty directory mounted after (so over) the home.
        let (args, _) = sandboxed_args(&machine, &cfg, &rules(&["~"], &[], true)).unwrap();
        let home = mount_at(&args, "--ro-bind", &machine.home, &machine.home).unwrap();
        let hidden = args
            .windows(2)
            .position(|w| w[0] == "--tmpfs" && Path::new(&w[1]) == machine.state)
            .expect("the state directory is covered");
        // Guards "outside code never sees AgentOS's files": the cover must
        // come after the home, or the home would be mounted over it.
        assert!(hidden > home);

        let refused = |read: &str| {
            sandboxed_args(&machine, &cfg, &rules(&[read], &[], true))
                .map(|_| ())
                .unwrap_err()
        };
        // A rule inside the state directory is refused outright.
        let inside = machine.state.join("pin.json");
        assert!(refused(inside.to_str().unwrap()).contains("AgentOS 自己的目录"));
        assert!(refused("~/.local/state/agentos").contains("AgentOS 自己的目录"));
        // So are relative paths, the whole system, and paths that do not exist.
        assert!(refused("docs").contains("绝对路径"));
        assert!(refused("~/../other").contains("不能含 .."));
        assert!(refused("/").contains("整个根目录"));
        assert!(refused("~/missing").contains("不能用"));

        // A control socket moved out of the state directory (and not yet
        // created) cannot be handed over by any rule that includes it.
        let contain = Containment::new(
            SandboxMode::Bubblewrap,
            Some(machine.home.clone()),
            &machine.state,
            &[machine.home.join("agentos.sock")],
        );
        let e = sandboxed("s", &cfg, &rules(&["~"], &[], true), &contain).unwrap_err();
        assert!(e.contains("控制 socket"), "{e}");
    }

    #[test]
    fn a_program_the_sandbox_cannot_see_is_named_not_started() {
        let machine = Machine::new("program");
        // Installed in the user's home, as nvm and uv install things.
        let bin = machine.home.join(".tool/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let program = bin.join("fake-mcp");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = ServerConfig {
            command: program.display().to_string(),
            ..ServerConfig::default()
        };
        let e = sandboxed_args(&machine, &cfg, &SandboxRules::default())
            .map(|_| ())
            .unwrap_err();
        // Guards a clear failure: the user learns which program is out of
        // sight and what to add, instead of a server that silently dies.
        assert!(e.contains("在沙箱里看不到"), "{e}");
        assert!(e.contains(program.to_str().unwrap()), "{e}");
        // With its directory given, it runs.
        let (args, _) = sandboxed_args(&machine, &cfg, &rules(&["~/.tool"], &[], true)).unwrap();
        assert!(args.iter().any(|arg| Path::new(arg) == program));

        let missing = ServerConfig {
            command: "agentos-no-such-mcp-server".into(),
            ..ServerConfig::default()
        };
        let e = sandboxed_args(&machine, &missing, &SandboxRules::default())
            .map(|_| ())
            .unwrap_err();
        assert!(e.contains("找不到命令"), "{e}");
        let relative = ServerConfig {
            command: "./server".into(),
            ..ServerConfig::default()
        };
        let e = sandboxed_args(&machine, &relative, &SandboxRules::default())
            .map(|_| ())
            .unwrap_err();
        assert!(e.contains("绝对路径"), "{e}");
    }

    #[tokio::test]
    async fn without_bubblewrap_a_server_starts_only_if_the_user_said_so() {
        let (mut cfg, dir) = fake_config("unboxed", None, false);
        cfg.servers.get_mut("Fake Srv").unwrap().sandbox = Sandbox::default();
        let mut reg = CapabilityRegistry::new();
        let notes = register_all(&mut reg, &cfg, &no_sandbox()).await;
        // Guards "outside code runs sandboxed": no sandbox here, and the user
        // did not say the server may go without, so it does not run at all.
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("没有可用的 bubblewrap 沙箱"),
            "{}",
            notes[0]
        );
        assert!(notes[0].contains("\"sandbox\": false"), "{}", notes[0]);
        assert!(reg.specs().is_empty());

        if python3() {
            cfg.servers.get_mut("Fake Srv").unwrap().sandbox = Sandbox::Off;
            let mut reg = CapabilityRegistry::new();
            let notes = register_all(&mut reg, &cfg, &no_sandbox()).await;
            assert_eq!(
                notes,
                vec!["MCP server Fake Srv：4 个工具（不在沙箱里，有你的全部权限；结果视为不可信）"]
            );
            assert_eq!(reg.specs().len(), 4);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A stdio MCP server with one tool that tries things from inside its
    /// sandbox and reports what happened, failures included.
    const PROBE_SERVER: &str = r#"
import json, os, socket, sys
def probe(a):
    op = a.get("op")
    try:
        if op == "read":
            with open(a["path"]) as f:
                return f.read()
        if op == "write":
            with open(a["path"], "w") as f:
                f.write(a.get("text", ""))
            return "ok"
        if op == "net":
            socket.create_connection(("1.1.1.1", 53), timeout=3).close()
            return "ok"
        if op == "env":
            return os.environ.get(a["name"], "none")
    except Exception as e:
        return "ERR " + type(e).__name__
    return "ERR unknown op"
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    m, p = msg["method"], msg.get("params", {})
    if m == "initialize":
        r = {"protocolVersion": p["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "probe", "version": "1"}}
    elif m == "tools/list":
        r = {"tools": [{"name": "probe", "description": "Try something", "inputSchema": {"type": "object"}}]}
    elif m == "tools/call":
        r = {"content": [{"type": "text", "text": probe(p.get("arguments", {}))}]}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "no such method"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": r}), flush=True)
"#;

    #[tokio::test]
    async fn a_sandboxed_server_sees_only_what_its_rules_allow() {
        if SandboxMode::detect() != SandboxMode::Bubblewrap {
            // CI installs bubblewrap so this boundary is always tested there.
            assert!(
                std::env::var_os("CI").is_none(),
                "bubblewrap must work in CI: the MCP sandbox is a security boundary"
            );
            eprintln!("bubblewrap not available, skipping");
            return;
        }
        if !python3() {
            return;
        }
        let machine = Machine::new("live");
        let code = machine.dir("code");
        std::fs::write(code.join("server.py"), PROBE_SERVER).unwrap();
        let shared = machine.dir("shared");
        std::fs::write(shared.join("readme.txt"), "shared text").unwrap();
        let out = machine.dir("out");
        let server = |rules: SandboxRules| McpConfig {
            servers: [(
                "probe".to_string(),
                ServerConfig {
                    command: "python3".into(),
                    args: vec!["-I".into(), code.join("server.py").display().to_string()],
                    sandbox: Sandbox::On(rules),
                    ..ServerConfig::default()
                },
            )]
            .into(),
        };
        let contain = machine.containment(SandboxMode::Bubblewrap);
        let code_dir = code.to_str().unwrap();
        let mut reg = CapabilityRegistry::new();
        let narrow = rules(
            &[code_dir, shared.to_str().unwrap()],
            &[out.to_str().unwrap()],
            false,
        );
        let notes = register_all(&mut reg, &server(narrow), &contain).await;
        assert_eq!(
            notes,
            vec!["MCP server probe：1 个工具（在沙箱里，不能联网；结果视为不可信）"]
        );
        let probe = reg
            .get(&CapabilityName::new("mcp.probe.probe"))
            .unwrap()
            .clone();
        let ask = |args: Value| {
            let probe = probe.clone();
            async move {
                probe.invoke(args).await.unwrap()["message"]
                    .as_str()
                    .unwrap()
                    .to_string()
            }
        };
        let path = |p: &Path| p.display().to_string();
        // Guards the user's files: the server's home is its own, so what is
        // in the real one is not there.
        let secret = path(&machine.home.join("secret.txt"));
        assert!(ask(json!({"op": "read", "path": secret}))
            .await
            .starts_with("ERR"));
        // Guards AgentOS's own files: the PIN, the policy and the control
        // socket live in the state directory, which is not there either.
        let pin = path(&machine.state.join("pin.json"));
        assert!(ask(json!({"op": "read", "path": pin}))
            .await
            .starts_with("ERR"));
        // A read rule reads and does not write; a write rule writes for real.
        let readme = path(&shared.join("readme.txt"));
        assert_eq!(
            ask(json!({"op": "read", "path": readme})).await,
            "shared text"
        );
        let scribble = path(&shared.join("x.txt"));
        assert!(ask(json!({"op": "write", "path": scribble, "text": "x"}))
            .await
            .starts_with("ERR"));
        let result = path(&out.join("result.txt"));
        assert_eq!(
            ask(json!({"op": "write", "path": result, "text": "hi"})).await,
            "ok"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("result.txt")).unwrap(),
            "hi"
        );
        // No network when switched off, and none of the daemon's environment.
        assert!(ask(json!({"op": "net"})).await.starts_with("ERR"));
        if std::env::var_os("CARGO_MANIFEST_DIR").is_some() {
            let leaked = ask(json!({"op": "env", "name": "CARGO_MANIFEST_DIR"})).await;
            assert_eq!(leaked, "none");
        }
        drop(reg);

        // Given the whole home, the server reads the user's files (they
        // asked for that) but still not AgentOS's state inside it.
        let mut reg = CapabilityRegistry::new();
        register_all(
            &mut reg,
            &server(rules(&[code_dir, "~"], &[], false)),
            &contain,
        )
        .await;
        let probe = reg
            .get(&CapabilityName::new("mcp.probe.probe"))
            .unwrap()
            .clone();
        let ask = |args: Value| {
            let probe = probe.clone();
            async move {
                probe.invoke(args).await.unwrap()["message"]
                    .as_str()
                    .unwrap()
                    .to_string()
            }
        };
        assert_eq!(
            ask(json!({"op": "read", "path": secret})).await,
            "the user's secret"
        );
        assert!(ask(json!({"op": "read", "path": pin}))
            .await
            .starts_with("ERR"));
    }

    #[tokio::test]
    async fn tools_from_a_stdio_server_become_capabilities() {
        if std::process::Command::new("python3")
            .arg("-V")
            .output()
            .is_err()
        {
            eprintln!("python3 not available, skipping");
            return;
        }
        std::env::set_var("SECRET_KEY", "sk-should-not-leak");
        let (cfg, dir) = fake_config("tools", None, false);
        let mut reg = CapabilityRegistry::new();
        let notes = register_all(&mut reg, &cfg, &no_sandbox()).await;
        assert_eq!(
            notes,
            vec!["MCP server Fake Srv：4 个工具（不在沙箱里，有你的全部权限；结果视为不可信）"]
        );

        let cap = |n: &str| reg.get(&CapabilityName::new(n)).expect(n).clone();
        // Unknown risk defaults to L2; destructive is raised to L3; the
        // user's per-tool L0 holds even though readOnlyHint is not needed.
        assert_eq!(cap("mcp.fake_srv.echo").spec().risk, RiskLevel::External);
        assert_eq!(
            cap("mcp.fake_srv.wipe_all").spec().risk,
            RiskLevel::Irreversible
        );
        assert_eq!(cap("mcp.fake_srv.env").spec().risk, RiskLevel::ReadOnly);
        assert!(cap("mcp.fake_srv.echo").untrusted_output());
        assert_eq!(
            cap("mcp.fake_srv.echo").spec().input_schema["properties"]["text"]["type"],
            "string"
        );

        let v = cap("mcp.fake_srv.echo")
            .invoke(json!({"text": "你好"}))
            .await
            .unwrap();
        assert_eq!(v["message"], "echo: 你好");
        // The original tool name (with "-") is what the server receives.
        let v = cap("mcp.fake_srv.wipe_all")
            .invoke(json!({}))
            .await
            .unwrap();
        assert_eq!(v["message"], "wiped");
        // Cleared environment: our secrets stay ours, configured env passes.
        let v = cap("mcp.fake_srv.env").invoke(json!({})).await.unwrap();
        assert_eq!(v["message"], "none/yes");
        let e = cap("mcp.fake_srv.fail")
            .invoke(json!({}))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("it broke"), "{e}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn trusted_servers_do_not_taint() {
        if std::process::Command::new("python3")
            .arg("-V")
            .output()
            .is_err()
        {
            return;
        }
        let (cfg, dir) = fake_config("trusted", Some("L1"), true);
        let mut reg = CapabilityRegistry::new();
        register_all(&mut reg, &cfg, &no_sandbox()).await;
        let echo = reg.get(&CapabilityName::new("mcp.fake_srv.echo")).unwrap();
        assert!(!echo.untrusted_output());
        assert_eq!(echo.spec().risk, RiskLevel::Reversible);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_missing_command_is_reported_not_fatal() {
        let cfg = McpConfig {
            servers: [(
                "nope".to_string(),
                ServerConfig {
                    command: "/nonexistent/mcp-server".into(),
                    sandbox: Sandbox::Off,
                    ..ServerConfig::default()
                },
            )]
            .into(),
        };
        let mut reg = CapabilityRegistry::new();
        let notes = register_all(&mut reg, &cfg, &no_sandbox()).await;
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("无法启动"), "{}", notes[0]);
        assert!(reg.specs().is_empty());
    }
}
