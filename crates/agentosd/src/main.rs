//! `agentosd`: the AgentOS P0 daemon.
//!
//! Reads intents as lines from stdin (standing in for keyboard / speech),
//! lets the butler agent act on them, and prints what it says to the console.
//! Every step is appended to the audit log.

use std::sync::Arc;

use agentos_audit::AuditLog;
use agentos_core::{AgentId, PerceptKind, Sensor};
use agentos_policy::PolicyEngine;
use agentosd::agents::AgentDirectory;
use agentosd::blackboard::{self, Blackboard};
use agentosd::builtin;
use agentosd::butler::{grant_manifest, Butler};
use agentosd::claude_planner::{ClaudeConfig, ClaudePlanner};
use agentosd::crash;
use agentosd::devices::{ConsoleOutput, LineSensor};
use agentosd::evolve::{self, LlmGenerator, NoGenerator, PluginGenerator};
use agentosd::expression::ExpressionHub;
use agentosd::homeassistant::{self, Hass, HassConfig};
use agentosd::identity::{self, PinCheck, PinVault};
use agentosd::judge::LlmJudge;
use agentosd::llm::TextModel;
use agentosd::mcp::{self, McpConfig};
use agentosd::openai_planner::{OpenAiConfig, OpenAiPlanner};
use agentosd::perception::PerceptionHub;
use agentosd::planner::{Planner, RulePlanner};
use agentosd::plugin::SandboxMode;
use agentosd::plugin_store::PluginManager;
use agentosd::registry::CapabilityRegistry;
use agentosd::shell_api::{self, ShellServer, SocketOutput};
use agentosd::tasks::TaskStore;
use agentosd::voice::{SpeechOutput, Voice, VoiceConfig, MIC_DEVICE};
use tokio::sync::mpsc;

fn arg(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == name {
            return args.next();
        }
    }
    None
}

/// `--audit`, `AGENTOS_AUDIT`, or `audit.jsonl` in the data directory.
fn audit_path(home: &std::path::Path) -> std::path::PathBuf {
    arg("--audit")
        .or_else(|| std::env::var("AGENTOS_AUDIT").ok())
        .filter(|p| !p.is_empty())
        .map(Into::into)
        .unwrap_or_else(|| home.join("audit.jsonl"))
}

/// Where AgentOS keeps its state (notes, plugins, audit log, ...): `--home`, `AGENTOS_HOME`,
/// `$XDG_DATA_HOME/agentos`, or `~/.local/share/agentos`.
fn home_dir() -> std::path::PathBuf {
    if let Some(h) = arg("--home").or_else(|| std::env::var("AGENTOS_HOME").ok()) {
        return h.into();
    }
    if let Ok(x) = std::env::var("XDG_DATA_HOME") {
        return std::path::Path::new(&x).join("agentos");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::Path::new(&home).join(".local/share/agentos")
}

/// `--listen <path>` or `AGENTOS_SOCKET`: serve graphical shells on a Unix socket.
fn socket_path() -> Option<std::path::PathBuf> {
    arg("--listen")
        .or_else(|| std::env::var("AGENTOS_SOCKET").ok())
        .filter(|p| !p.is_empty())
        .map(Into::into)
}

/// A planner, the same model as plain text (for writing plugins), and a label.
type Models = (Box<dyn Planner>, Option<Arc<dyn TextModel>>, String);

fn rule() -> Models {
    (Box::new(RulePlanner), None, "规则规划器".into())
}

fn claude(cfg: ClaudeConfig) -> Models {
    match (ClaudePlanner::new(cfg.clone()), ClaudePlanner::new(cfg)) {
        (Ok(p), Ok(t)) => {
            let label = format!("Claude 规划器（{}）", p.model());
            (Box::new(p), Some(Arc::new(t)), label)
        }
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("无法创建 HTTP 客户端：{e}，改用规则规划器");
            rule()
        }
    }
}

fn openai(cfg: OpenAiConfig) -> Models {
    match (OpenAiPlanner::new(cfg.clone()), OpenAiPlanner::new(cfg)) {
        (Ok(p), Ok(t)) => {
            let label = format!("OpenAI 兼容规划器（{} @ {}）", p.model(), p.base_url());
            (Box::new(p), Some(Arc::new(t)), label)
        }
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("无法创建 HTTP 客户端：{e}，改用规则规划器");
            rule()
        }
    }
}

/// Without an LLM planner the generator still uses whichever model is configured.
fn fallback_text_model() -> Option<Arc<dyn TextModel>> {
    if let Some(cfg) = ClaudeConfig::from_env() {
        return ClaudePlanner::new(cfg)
            .ok()
            .map(|p| Arc::new(p) as Arc<dyn TextModel>);
    }
    OpenAiConfig::from_env()
        .and_then(|cfg| OpenAiPlanner::new(cfg).ok())
        .map(|p| Arc::new(p) as Arc<dyn TextModel>)
}

/// `--planner claude|openai|rule` (or `AGENTOS_PLANNER`). Without a choice:
/// Claude if `ANTHROPIC_API_KEY` is set, else an OpenAI-compatible endpoint if
/// configured, else the offline rule planner.
fn make_planner() -> Models {
    let choice = arg("--planner").or_else(|| std::env::var("AGENTOS_PLANNER").ok());
    match choice.as_deref() {
        Some("rule") => rule(),
        Some("claude") => match ClaudeConfig::from_env() {
            Some(cfg) => claude(cfg),
            None => {
                eprintln!("未设置 ANTHROPIC_API_KEY，改用规则规划器");
                rule()
            }
        },
        Some("openai") => match OpenAiConfig::from_env() {
            Some(cfg) => openai(cfg),
            None => {
                eprintln!(
                    "OpenAI 兼容规划器需要 OPENAI_MODEL，以及 OPENAI_API_KEY 或 OPENAI_BASE_URL，改用规则规划器"
                );
                rule()
            }
        },
        Some(other) => {
            eprintln!("未知的规划器 {other}（可选 claude、openai、rule），改用规则规划器");
            rule()
        }
        None => {
            if let Some(cfg) = ClaudeConfig::from_env() {
                claude(cfg)
            } else if let Some(cfg) = OpenAiConfig::from_env() {
                openai(cfg)
            } else {
                rule()
            }
        }
    }
}

/// `agentosd --set-pin`: read a new PIN (twice) from stdin and store it.
/// `agentosd --clear-pin`: remove it, so L3 steps need only a confirmation.
fn pin_command(home: &std::path::Path) -> Option<std::io::Result<()>> {
    let all: Vec<String> = std::env::args().collect();
    let path = home.join("pin.json");
    if all.iter().any(|a| a == "--clear-pin") {
        return Some(match std::fs::remove_file(&path) {
            Ok(()) => {
                println!("已删除 PIN（{}）。", path.display());
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("没有设置过 PIN。");
                Ok(())
            }
            Err(e) => Err(e),
        });
    }
    if !all.iter().any(|a| a == "--set-pin") {
        return None;
    }
    Some((|| {
        let mut lines = std::io::stdin().lines();
        let mut ask = |prompt: &str| -> std::io::Result<String> {
            eprint!("{prompt}");
            Ok(lines
                .next()
                .transpose()?
                .unwrap_or_default()
                .trim()
                .to_string())
        };
        let first = ask("新 PIN（4 到 12 位数字）：")?;
        if !identity::valid_pin(&first) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "PIN 必须是 4 到 12 位数字",
            ));
        }
        if ask("再输入一次：")? != first {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "两次输入的 PIN 不一致",
            ));
        }
        PinVault::open(&path)?.set(&first)?;
        println!(
            "PIN 已设置（{}）。不可逆的操作现在需要输入它。",
            path.display()
        );
        Ok(())
    })())
}

/// `agentosd --verify-audit`: check the audit log's hash chain. Exits 1 if
/// an entry was changed, removed or inserted.
fn verify_audit(path: &std::path::Path) -> std::io::Result<()> {
    let r = AuditLog::verify(path)?;
    println!("审计日志：{}", path.display());
    println!(
        "  {} 条记录，{} 条带哈希链{}",
        r.lines,
        r.chained,
        if r.legacy > 0 {
            format!("，{} 条是启用哈希链之前写的", r.legacy)
        } else {
            String::new()
        }
    );
    if !r.damaged.is_empty() {
        println!(
            "  第 {:?} 行写到一半（多半是断电或崩溃），已跳过",
            r.damaged
        );
    }
    println!("  链头：{}", r.head);
    if r.ok() {
        println!("  哈希链完整。");
        Ok(())
    } else {
        println!(
            "  哈希链在第 {:?} 行断开：它之前的记录被修改、删除或插入过。",
            r.breaks
        );
        std::process::exit(1);
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let home = home_dir();
    std::fs::create_dir_all(&home)?;
    if let Some(r) = pin_command(&home) {
        return r;
    }
    if std::env::args().any(|a| a == "--verify-audit") {
        return verify_audit(&audit_path(&home));
    }
    crash::install(&home);
    let audit = Arc::new(AuditLog::open(audit_path(&home))?);
    if let Some(summary) = crash::take_last(&home) {
        eprintln!(
            "上次 agentosd 异常退出：{summary}\n  完整报告：{}（也在 journalctl -t agentosd 里）",
            home.join("crash.prev.txt").display()
        );
        audit.append(agentos_audit::AuditEvent::DaemonCrashed { summary })?;
    }
    let pins = Arc::new(PinVault::open(home.join("pin.json"))?.with_audit(audit.clone()));

    let (planner, text_model, planner_label) = make_planner();

    let mut registry = CapabilityRegistry::new();
    builtin::register_with_notes_file(&mut registry, home.join("notes.json"))?;

    let sandbox = SandboxMode::detect();
    let plugins = Arc::new(PluginManager::open(home.join("evolve"), sandbox)?);
    let text_model = text_model.or_else(fallback_text_model);
    let generator: Arc<dyn PluginGenerator> = match text_model.clone() {
        Some(m) => Arc::new(LlmGenerator::new(m)),
        None => Arc::new(NoGenerator),
    };
    evolve::register_all(&mut registry, plugins.clone(), generator);

    let board = Blackboard::open(home.join("blackboard.json"))?;
    blackboard::register_all(&mut registry, board.clone());
    // <home>/mcp.json: tools from MCP servers.
    match McpConfig::load(&home.join("mcp.json")) {
        Ok(cfg) => {
            for n in mcp::register_all(&mut registry, &cfg).await {
                eprintln!("{n}");
            }
        }
        Err(e) => eprintln!("{e}"),
    }
    // HASS_URL + HASS_TOKEN: the smart home, through Home Assistant.
    let home_label = match HassConfig::from_env().map(Hass::new) {
        Some(Ok(h)) => {
            let label = format!("Home Assistant（{}）", h.base_url());
            homeassistant::register_all(&mut registry, Arc::new(h));
            Some(label)
        }
        Some(Err(e)) => {
            eprintln!("无法创建 Home Assistant 客户端：{e}");
            None
        }
        None => None,
    };

    let butler_id = AgentId::new("butler");
    let mut policy = PolicyEngine::new();
    let manifest: Vec<_> = registry.specs().into_iter().map(|s| s.name).collect();
    grant_manifest(&mut policy, &butler_id, &manifest);

    let bus = shell_api::event_bus();
    // Sensors run as independent tasks and feed one percept channel.
    let (tx, mut rx) = mpsc::channel(64);

    // Voice only works through a graphical shell (it owns the talk button).
    let voice = socket_path().map(|_| {
        let (cfg, notes) = VoiceConfig::from_env();
        for n in notes {
            eprintln!("{n}");
        }
        Voice::new(cfg, bus.clone(), tx.clone(), audit.clone())
    });

    let mut expr = ExpressionHub::new(audit.clone());
    expr.add_output(Arc::new(ConsoleOutput::default()));
    expr.add_output(Arc::new(SocketOutput::new(bus.clone())));
    if let Some(v) = &voice {
        expr.add_output(Arc::new(SpeechOutput::new(v.clone())));
    }

    let mut agents = AgentDirectory::with_builtin();
    for note in agents.load_dir(&home.join("agents")) {
        eprintln!("{note}");
    }
    let agent_names: Vec<String> = agents
        .all()
        .map(|m| format!("{}（@{}）", m.name, m.id))
        .collect();

    let butler = Butler::new(butler_id, registry, policy, planner, audit.clone(), expr)
        .with_plugins(plugins.clone())
        .with_events(bus.clone())
        .with_agents(agents)
        .with_blackboard(board)
        .with_pin_vault(pins.clone());
    // AGENTOS_JUDGE=1: a model reviews allowed L1+ steps before they run.
    let judge_on = std::env::var("AGENTOS_JUDGE").is_ok_and(|v| v == "1" || v == "on");
    let butler = match (&text_model, judge_on) {
        (Some(m), true) => butler.with_judge(Arc::new(LlmJudge::new(m.clone()))),
        (None, true) => {
            eprintln!("AGENTOS_JUDGE 需要配置一个模型，裁判未启用");
            butler
        }
        _ => butler,
    };
    let (mut butler, task_notes) = butler.with_task_store(TaskStore::open(home.join("tasks"))?);
    for n in task_notes {
        eprintln!("{n}");
    }

    if let Some(path) = socket_path() {
        let listener = shell_api::bind(&path)?;
        println!("图形界面接口：{}", path.display());
        let server = ShellServer {
            hello: serde_json::json!({
                "type": "hello",
                "planner": planner_label,
                "version": env!("CARGO_PKG_VERSION"),
                "voice": voice.as_ref().map(|v| v.describe()),
            }),
            bus,
            audit: audit.path().to_path_buf(),
            percepts: tx.clone(),
            voice: voice.clone(),
            pins: Some(pins.clone()),
        };
        tokio::spawn(server.serve(listener));
    }
    let mut sensor = LineSensor::stdin();
    let console_pins = pins.clone();
    tokio::spawn(async move {
        while let Some(p) = sensor.next().await {
            // "pin 1234" on the console vouches for the next L3 answer. It is
            // handled here so the PIN never becomes an intent (or audit text).
            if let PerceptKind::Text { text, .. } = &p.kind {
                if let Some(pin) = text.trim().strip_prefix("pin ") {
                    let check = console_pins.verify_for(None, pin.trim());
                    println!(
                        "{}",
                        match check {
                            PinCheck::Ok => "PIN 正确，现在回复 y 确认（1 分钟内有效）。".into(),
                            other => other.message(),
                        }
                    );
                    continue;
                }
            }
            if tx.send(p).await.is_err() {
                break;
            }
        }
    });

    println!(
        "AgentOS P0 · 管家 Agent 已就绪（{planner_label}，审计日志：{}）。输入你的意图，Ctrl-D 退出。",
        audit.path().display()
    );
    println!(
        "自我进化：插件目录 {}，{}。",
        plugins.root().display(),
        sandbox.label()
    );
    println!(
        "可调度的子 Agent：{}；自定义 Agent 放在 {}。任务和黑板保存在 {}，重启后接着做。",
        agent_names.join("、"),
        home.join("agents").display(),
        home.display()
    );
    if let Some(v) = &voice {
        let d = v.describe();
        let label = |k: &str| d[k].as_str().unwrap_or("未配置").to_string();
        println!(
            "语音：识别 {}，播报 {}（{}）。",
            label("input_label"),
            label("output_label"),
            match d["speak"].as_str() {
                Some("always") => "总是朗读回复",
                Some("off") => "不朗读",
                _ => "语音提问时朗读回复",
            }
        );
    }
    match &home_label {
        Some(l) => println!("智能家居：{l}。"),
        None => println!("智能家居：未配置（设置 HASS_URL 和 HASS_TOKEN 接入 Home Assistant）。"),
    }
    if butler.pending_resume() > 0 {
        println!(
            "有 {} 个重启前未完成的任务，继续执行。",
            butler.pending_resume()
        );
    }
    butler.resume().await;
    let mut perception = PerceptionHub::new();
    while let Some(p) = rx.recv().await {
        if let Some(intent) = perception.to_intent(p) {
            if let (Some(v), MIC_DEVICE) = (&voice, intent.source.as_str()) {
                v.mark_voice_intent(intent.id);
            }
            butler.handle(intent).await;
        }
    }
    Ok(())
}
