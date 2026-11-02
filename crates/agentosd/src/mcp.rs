//! MCP servers as capabilities.
//!
//! Any [Model Context Protocol](https://modelcontextprotocol.io) server that
//! speaks stdio can lend its tools to the butler. Each tool becomes a
//! capability named `mcp.<server>.<tool>` and goes through the policy engine
//! like everything else.
//!
//! Servers are listed in `<home>/mcp.json`, in the same shape Claude Desktop
//! uses, plus AgentOS's own risk settings:
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
//!       "trusted": true
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

use std::collections::HashMap;
use std::path::Path;
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

const PROTOCOL_VERSION: &str = "2025-06-18";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Longest tool name the Claude and OpenAI APIs accept.
const MAX_TOOL_NAME: usize = 64;
/// Longest line read from a server, so a broken one cannot exhaust memory.
const MAX_LINE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
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

/// A running stdio MCP server.
pub struct McpClient {
    name: String,
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    _child: Mutex<Child>,
}

impl McpClient {
    /// Start the server, initialize the session and return the client.
    pub async fn start(name: &str, cfg: &ServerConfig) -> Result<Arc<Self>, String> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        for k in ["PATH", "HOME", "LANG", "USER", "TMPDIR", "XDG_RUNTIME_DIR"] {
            if let Ok(v) = std::env::var(k) {
                cmd.env(k, v);
            }
        }
        cmd.envs(&cfg.env);
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
        let sent = self
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        if let Err(e) = sent {
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(format!("MCP server {} 已退出", self.name)),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                Err(format!("MCP server {} 响应超时", self.name))
            }
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
pub async fn register_all(reg: &mut CapabilityRegistry, cfg: &McpConfig) -> Vec<String> {
    let mut notes = Vec::new();
    let mut names: Vec<_> = cfg.servers.keys().cloned().collect();
    names.sort();
    for name in names {
        let sc = &cfg.servers[&name];
        if sc.disabled {
            continue;
        }
        let client = match McpClient::start(&name, sc).await {
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
        notes.push(format!(
            "MCP server {name}：{} 个工具{}",
            added.len(),
            if sc.trusted {
                ""
            } else {
                "（结果视为不可信）"
            }
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
        };
        (
            McpConfig {
                servers: [("Fake Srv".to_string(), sc)].into(),
            },
            dir,
        )
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
        let tool = crate::planner::tool_name(&long);
        assert!(tool.len() <= MAX_TOOL_NAME, "{tool}");
        assert_eq!(crate::planner::capability_name(&tool), long);
        assert!(capability_for("", "x").is_none());
        assert!(capability_for("srv", "!!!").is_none());
    }

    #[test]
    fn risk_comes_from_the_user_and_hints_only_raise_it() {
        let sc = ServerConfig {
            command: String::new(),
            args: vec![],
            env: HashMap::new(),
            risk: Some("L1".into()),
            tools: [("read".to_string(), "L0".to_string())].into(),
            trusted: false,
            disabled: false,
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
        let notes = register_all(&mut reg, &cfg).await;
        assert_eq!(
            notes,
            vec!["MCP server Fake Srv：4 个工具（结果视为不可信）"]
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
        register_all(&mut reg, &cfg).await;
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
                    args: vec![],
                    env: HashMap::new(),
                    risk: None,
                    tools: HashMap::new(),
                    trusted: false,
                    disabled: false,
                },
            )]
            .into(),
        };
        let mut reg = CapabilityRegistry::new();
        let notes = register_all(&mut reg, &cfg).await;
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("无法启动"), "{}", notes[0]);
        assert!(reg.specs().is_empty());
    }
}
