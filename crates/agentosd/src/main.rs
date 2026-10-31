//! `agentosd`: the AgentOS P0 daemon.
//!
//! Reads intents as lines from stdin (standing in for keyboard / speech),
//! lets the butler agent act on them, and prints what it says to the console.
//! Every step is appended to the audit log.

use std::sync::Arc;

use agentos_audit::AuditLog;
use agentos_core::{AgentId, Sensor};
use agentos_policy::PolicyEngine;
use agentosd::builtin;
use agentosd::butler::{grant_manifest, Butler};
use agentosd::claude_planner::{ClaudeConfig, ClaudePlanner};
use agentosd::devices::{ConsoleOutput, LineSensor};
use agentosd::evolve::{self, LlmGenerator, NoGenerator, PluginGenerator};
use agentosd::expression::ExpressionHub;
use agentosd::llm::TextModel;
use agentosd::openai_planner::{OpenAiConfig, OpenAiPlanner};
use agentosd::perception::PerceptionHub;
use agentosd::planner::{Planner, RulePlanner};
use agentosd::plugin::SandboxMode;
use agentosd::plugin_store::PluginManager;
use agentosd::registry::CapabilityRegistry;
use agentosd::shell_api::{self, ShellServer, SocketOutput};
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

fn audit_path() -> String {
    arg("--audit")
        .or_else(|| std::env::var("AGENTOS_AUDIT").ok())
        .unwrap_or_else(|| "agentos-audit.jsonl".into())
}

/// Where AgentOS keeps its state (plugins, ...): `--home`, `AGENTOS_HOME`,
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

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let audit = Arc::new(AuditLog::open(audit_path())?);

    let (planner, text_model, planner_label) = make_planner();

    let mut registry = CapabilityRegistry::new();
    builtin::register_all(&mut registry);

    let home = home_dir();
    let sandbox = SandboxMode::detect();
    let plugins = Arc::new(PluginManager::open(home.join("evolve"), sandbox)?);
    let generator: Arc<dyn PluginGenerator> = match text_model.or_else(fallback_text_model) {
        Some(m) => Arc::new(LlmGenerator::new(m)),
        None => Arc::new(NoGenerator),
    };
    evolve::register_all(&mut registry, plugins.clone(), generator);

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

    let mut butler = Butler::new(butler_id, registry, policy, planner, audit.clone(), expr)
        .with_plugins(plugins.clone())
        .with_events(bus.clone());

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
        };
        tokio::spawn(server.serve(listener));
    }
    let mut sensor = LineSensor::stdin();
    tokio::spawn(async move {
        while let Some(p) = sensor.next().await {
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
