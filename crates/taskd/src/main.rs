//! AgentOS system service. Task ownership is independent of client lifetimes.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentos_audit::AuditLog;
use agentos_core::AgentId;
use agentos_planner::claude::{ClaudeConfig, ClaudePlanner};
use agentos_planner::llm::TextModel;
use agentos_planner::openai::{OpenAiConfig, OpenAiPlanner};
use agentos_planner::{Planner, RulePlanner};
use agentos_policy::PolicyEngine;
use agentos_runtime::TaskStore;
use agentos_services::agents::AgentDirectory;
use agentos_services::blackboard::{self, Blackboard};
use agentos_services::evolve::{self, LlmGenerator, NoGenerator, PluginGenerator};
use agentos_services::expression::ExpressionHub;
use agentos_services::homeassistant::{self, Hass, HassConfig};
use agentos_services::identity::{self, PinVault};
use agentos_services::judge::LlmJudge;
use agentos_services::mcp::{self, McpConfig};
use agentos_services::plugin::SandboxMode;
use agentos_services::plugin_store::PluginManager;
use agentos_services::registry::CapabilityRegistry;
use agentos_taskd::builtin;
use agentos_taskd::butler::{grant_manifest, Budget, Butler};
use agentos_taskd::callers::CallerStore;
use agentos_taskd::crash;
use agentos_taskd::preauth::PreauthStore;
use agentos_taskd::service::Service;
use agentos_taskd::voice::{SpeechOutput, Voice, VoiceConfig};

const HELP: &str = "\
用法：agentos-taskd [选项]

  --planner rule|claude|openai  规划器（或环境变量 AGENTOS_PLANNER）
  --state-dir 路径              状态目录（或 AGENTOS_STATE_DIR；默认 ~/.local/state/agentos）
  --socket 路径                 控制 socket（或 AGENTOS_TASK_SOCKET；默认在状态目录下）
  --audit 路径                  审计日志（或 AGENTOS_TASK_AUDIT；默认在状态目录下）
  --set-pin                     设置不可逆操作的 PIN，然后退出
  --clear-pin                   取消 PIN，然后退出
  --verify-audit                校验审计日志的哈希链，然后退出
  --help, -h                    查看帮助
  --version, -V                 查看版本

状态目录里的可选配置：mcp.json（MCP server）、agents/*.json（自定义子 Agent）。
环境变量：HASS_URL 与 HASS_TOKEN 接入 Home Assistant；AGENTOS_JUDGE=1 启用裁判。

在另一终端运行 agentos workbench 连接工作台。
服务不读取 stdin；客户端退出后任务继续。SIGINT/SIGTERM 停止服务。";

#[derive(Default, Debug, PartialEq)]
enum Mode {
    #[default]
    Serve,
    SetPin,
    ClearPin,
    VerifyAudit,
}

#[derive(Default, Debug)]
struct Options {
    planner: Option<String>,
    audit: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    socket: Option<PathBuf>,
    mode: Mode,
}

/// `Ok(Err(text))` is text to print before exiting successfully.
fn options(args: impl IntoIterator<Item = String>) -> io::Result<Result<Options, String>> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    while let Some(name) = args.next() {
        match name.as_str() {
            "--help" | "-h" => {
                return Ok(Err(format!(
                    "AgentOS v{} · 后台任务服务\n\n{HELP}",
                    env!("CARGO_PKG_VERSION")
                )))
            }
            "--version" | "-V" => {
                return Ok(Err(format!("agentos-taskd {}", env!("CARGO_PKG_VERSION"))))
            }
            "--set-pin" => options.mode = Mode::SetPin,
            "--clear-pin" => options.mode = Mode::ClearPin,
            "--verify-audit" => options.mode = Mode::VerifyAudit,
            "--planner" | "--audit" | "--state-dir" | "--socket" => {
                let value = args
                    .next()
                    .filter(|s| !s.is_empty() && !s.starts_with("--"))
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, format!("{name} 缺少值"))
                    })?;
                match name.as_str() {
                    "--planner" => options.planner = Some(value),
                    "--audit" => options.audit = Some(value.into()),
                    "--state-dir" => options.state_dir = Some(value.into()),
                    _ => options.socket = Some(value.into()),
                }
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("未知参数：{name}，使用 --help 查看用法"),
                ))
            }
        }
    }
    Ok(Ok(options))
}

/// A planner, the same model as plain text (for the plugin generator and the
/// judge), and a label.
type Models = (Box<dyn Planner>, Option<Arc<dyn TextModel>>, String);

fn rule() -> Models {
    (Box::new(RulePlanner), None, "离线规则规划器".into())
}

fn make_planner(choice: Option<String>) -> io::Result<Models> {
    let choice = choice.or_else(|| std::env::var("AGENTOS_PLANNER").ok());
    let error = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message);
    let claude = |cfg: ClaudeConfig| -> io::Result<Models> {
        let planner = ClaudePlanner::new(cfg.clone()).map_err(io::Error::other)?;
        let text = ClaudePlanner::new(cfg).map_err(io::Error::other)?;
        let label = format!("Claude 规划器（{}）", planner.model());
        Ok((Box::new(planner), Some(Arc::new(text)), label))
    };
    let openai = |cfg: OpenAiConfig| -> io::Result<Models> {
        let planner = OpenAiPlanner::new(cfg.clone()).map_err(io::Error::other)?;
        let text = OpenAiPlanner::new(cfg).map_err(io::Error::other)?;
        let label = format!(
            "OpenAI 兼容规划器（{} @ {}）",
            planner.model(),
            planner.base_url()
        );
        Ok((Box::new(planner), Some(Arc::new(text)), label))
    };
    match choice.as_deref() {
        Some("rule") => Ok(rule()),
        Some("claude") => claude(
            ClaudeConfig::from_env().ok_or_else(|| error("Claude 规划器缺少 ANTHROPIC_API_KEY"))?,
        ),
        Some("openai") => openai(OpenAiConfig::from_env().ok_or_else(|| {
            error("OpenAI 兼容规划器需要 OPENAI_MODEL，以及 OPENAI_API_KEY 或 OPENAI_BASE_URL")
        })?),
        Some(other) => Err(error(&format!(
            "未知规划器 {other}，可选 rule、claude、openai"
        ))),
        None => {
            if let Some(cfg) = ClaudeConfig::from_env() {
                claude(cfg)
            } else if let Some(cfg) = OpenAiConfig::from_env() {
                openai(cfg)
            } else {
                Ok(rule())
            }
        }
    }
}

/// With the offline planner, the plugin generator and the judge still use
/// whichever model is configured.
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

/// Read a new PIN (twice) from stdin and store it.
fn set_pin(path: &Path) -> io::Result<()> {
    let mut lines = io::stdin().lines();
    let mut ask = |prompt: &str| -> io::Result<String> {
        eprint!("{prompt}");
        Ok(lines
            .next()
            .transpose()?
            .unwrap_or_default()
            .trim()
            .to_string())
    };
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message);
    let first = ask("新 PIN（4 到 12 位数字）：")?;
    if !identity::valid_pin(&first) {
        return Err(invalid("PIN 必须是 4 到 12 位数字"));
    }
    if ask("再输入一次：")? != first {
        return Err(invalid("两次输入的 PIN 不一致"));
    }
    PinVault::open(path)?.set(&first)?;
    println!(
        "PIN 已设置（{}）。不可逆的操作现在需要输入它才能批准。",
        path.display()
    );
    Ok(())
}

fn clear_pin(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => println!("已删除 PIN（{}）。不可逆的操作将被拒绝。", path.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => println!("没有设置过 PIN。"),
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Report on the audit log's hash chain. Exits 1 if an entry was changed,
/// removed or inserted.
fn verify_audit(path: &Path) -> io::Result<()> {
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
async fn main() -> io::Result<()> {
    let options = match options(std::env::args().skip(1))? {
        Ok(options) => options,
        Err(text) => {
            println!("{text}");
            return Ok(());
        }
    };
    let state_dir = options
        .state_dir
        .map_or_else(agentos_ipc::default_state_dir, Ok)?;
    let audit_path = options
        .audit
        .or_else(|| {
            std::env::var_os("AGENTOS_TASK_AUDIT")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| state_dir.join("audit.jsonl"));
    let pin_path = state_dir.join("pin.json");
    match options.mode {
        Mode::Serve => {}
        Mode::SetPin => {
            agentos_runtime::create_private_dir(&state_dir)?;
            return set_pin(&pin_path);
        }
        Mode::ClearPin => return clear_pin(&pin_path),
        Mode::VerifyAudit => return verify_audit(&audit_path),
    }
    // From here on a panic is reported and ends the service; see crash.rs.
    crash::install(&state_dir);
    let (planner, text_model, label) = make_planner(options.planner)?;
    let socket = options
        .socket
        .unwrap_or_else(|| agentos_ipc::default_socket_path(&state_dir));
    let tasks = Arc::new(TaskStore::open(&state_dir)?);
    // A damaged or altered log must not keep the machine from starting: it
    // is set aside untouched and a new one begins, saying so.
    let (audit, set_aside) = AuditLog::open_or_set_aside(audit_path)?;
    if let Some(old) = set_aside {
        eprintln!(
            "审计日志无法继续写入（已损坏或被改动），原文件保留为 {}，已开始新的日志。",
            old.display()
        );
    }
    let audit = Arc::new(audit);
    if let Some(summary) = crash::take_last(&state_dir) {
        eprintln!(
            "上次 agentos-taskd 异常退出：{summary}\n  完整报告：{}（也在 journalctl -t agentos-taskd 里）",
            state_dir.join("crash.prev.txt").display()
        );
        audit.append(agentos_audit::AuditEvent::DaemonCrashed { summary })?;
    }
    let pins = Arc::new(PinVault::open(&pin_path)?.with_audit(audit.clone()));

    let mut registry = CapabilityRegistry::new();
    builtin::register_persistent(&mut registry, state_dir.join("notes.json"))?;
    let board = Blackboard::open(state_dir.join("blackboard.json"))?;
    blackboard::register_all(&mut registry, board.clone());

    // Self-evolution: plugins the system wrote for itself, sandboxed.
    let sandbox = SandboxMode::detect();
    let plugins = Arc::new(PluginManager::open(state_dir.join("evolve"), sandbox)?);
    let text_model = text_model.or_else(fallback_text_model);
    let generator: Arc<dyn PluginGenerator> = match text_model.clone() {
        Some(model) => Arc::new(LlmGenerator::new(model)),
        None => Arc::new(NoGenerator),
    };
    evolve::register_all(&mut registry, plugins.clone(), generator);

    // <state>/mcp.json: tools from MCP servers.
    match McpConfig::load(&state_dir.join("mcp.json")) {
        Ok(config) => {
            for note in mcp::register_all(&mut registry, &config).await {
                eprintln!("{note}");
            }
        }
        Err(e) => eprintln!("{e}"),
    }
    // HASS_URL + HASS_TOKEN: the smart home, through Home Assistant.
    let home = match HassConfig::from_env().map(Hass::new) {
        Some(Ok(hass)) => {
            let label = format!("Home Assistant（{}）", hass.base_url());
            homeassistant::register_all(&mut registry, Arc::new(hass));
            label
        }
        Some(Err(e)) => format!("无法创建 Home Assistant 客户端：{e}"),
        None => "未配置".into(),
    };

    let butler_id = AgentId::new("butler");
    let mut policy = PolicyEngine::new();
    let manifest: Vec<_> = registry.specs().into_iter().map(|s| s.name).collect();
    grant_manifest(&mut policy, &butler_id, &manifest);

    let mut agents = AgentDirectory::with_builtin();
    for note in agents.load_dir(&state_dir.join("agents")) {
        eprintln!("{note}");
    }
    let agent_names: Vec<String> = agents
        .all()
        .map(|m| format!("{}（@{}）", m.name, m.id))
        .collect();

    let budget = task_budget(|name| std::env::var(name).ok())
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    // Push-to-talk and read-aloud: configured in the environment, off unless
    // chosen, and never sending audio anywhere the human did not pick.
    let (voice_config, voice_notes) = VoiceConfig::from_env();
    for note in voice_notes {
        eprintln!("{note}");
    }
    let voice = Voice::new(voice_config, audit.clone());
    let mut expr = ExpressionHub::new(audit.clone());
    if voice.output_enabled() {
        expr.add_output(Arc::new(SpeechOutput::new(voice.clone())));
    }
    let mut butler = Butler::with_store(
        butler_id,
        registry,
        policy,
        planner,
        audit.clone(),
        expr,
        tasks,
    )
    .with_agents(agents)
    .with_blackboard(board)
    .with_pin_vault(pins.clone())
    .with_plugins(plugins)
    .with_budget(budget)
    .with_preauthorizations(Arc::new(PreauthStore::open(
        state_dir.join("preauthorizations.json"),
    )?))
    .with_callers(Arc::new(CallerStore::open(state_dir.join("callers.json"))?));
    // AGENTOS_JUDGE=1: a model reviews allowed L1+ steps before they run.
    let judge_on = std::env::var("AGENTOS_JUDGE").is_ok_and(|v| v == "1" || v == "on");
    let judge = match (&text_model, judge_on) {
        (Some(model), true) => {
            butler = butler.with_judge(Arc::new(LlmJudge::new(model.clone())));
            "已启用"
        }
        (None, true) => "需要配置一个模型，未启用",
        _ => "未启用",
    };

    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let voice_label = voice_label(&voice.status());
    let server = Service::with_butler(butler, audit, Some(pins.clone()))
        .with_voice(voice)
        .bind(&socket)
        .await?;
    println!(
        "AgentOS v{} · {label}\n状态目录：{}\nUnix socket：{}",
        env!("CARGO_PKG_VERSION"),
        state_dir.display(),
        server.path().display()
    );
    println!(
        "子 Agent：{}\n插件：{}\n智能家居：{home}\n语音：{voice_label}\n裁判：{judge}\n每个任务的预算：{}\nPIN：{}",
        agent_names.join("、"),
        sandbox.label(),
        budget_label(budget),
        if pins.is_set() {
            "已设置，不可逆的操作批准时需要输入"
        } else {
            "未设置，不可逆的操作会被拒绝（agentos-taskd --set-pin）"
        }
    );
    println!("后台服务已就绪。使用 agentos --socket <路径> workbench 连接。");
    server
        .run_until(async move {
            tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
        })
        .await
}

/// The console line for voice: what recognizes and reads, and where audio goes.
fn voice_label(status: &agentos_ipc::VoiceStatus) -> String {
    let input = match (&status.input_label, &status.destination) {
        (Some(label), Some(to)) => format!("识别 {label}（音频发往 {to}）"),
        _ => "识别未配置".into(),
    };
    let output = match &status.output_label {
        Some(label) => format!("朗读 {label}（{}）", status.speak.as_str()),
        None => "朗读未配置".into(),
    };
    format!("{input}；{output}")
}

/// What a task may consume, from the environment: `AGENTOS_TASK_TOKENS` and
/// `AGENTOS_TASK_SECONDS`. Unset keeps the default; `0` removes the limit. A
/// value that is not a number is an error rather than silently no limit.
fn task_budget(var: impl Fn(&str) -> Option<String>) -> Result<Budget, String> {
    let limit = |name: &str| -> Result<Option<Option<u64>>, String> {
        match var(name).as_deref().map(str::trim) {
            None | Some("") => Ok(None),
            Some(text) => text
                .parse::<u64>()
                .map(|value| Some((value > 0).then_some(value)))
                .map_err(|_| format!("{name} 应为非负整数（0 表示不限制）：{text}")),
        }
    };
    let mut budget = Budget::default();
    if let Some(tokens) = limit("AGENTOS_TASK_TOKENS")? {
        budget.tokens = tokens;
    }
    if let Some(seconds) = limit("AGENTOS_TASK_SECONDS")? {
        budget.active = seconds.map(std::time::Duration::from_secs);
    }
    Ok(budget)
}

fn budget_label(budget: Budget) -> String {
    let limit = |value: Option<String>| value.unwrap_or_else(|| "不限".into());
    format!(
        "{} 轮 · token {} · 执行时长 {}",
        budget.rounds,
        limit(budget.tokens.map(|tokens| tokens.to_string())),
        limit(budget.active.map(|time| format!("{} 秒", time.as_secs()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> io::Result<Result<Options, String>> {
        options(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn help_and_version_print_instead_of_starting_the_service() {
        let help = parse(&["--help"]).unwrap().unwrap_err();
        assert!(help.contains("用法：agentos-taskd") && help.contains("--set-pin"));
        assert_eq!(
            parse(&["-V"]).unwrap().unwrap_err(),
            format!("agentos-taskd {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn maintenance_modes_and_paths_are_parsed_and_mistakes_rejected() {
        let options = parse(&["--state-dir", "/tmp/s", "--planner", "rule", "--set-pin"])
            .unwrap()
            .unwrap();
        assert_eq!(options.mode, Mode::SetPin);
        assert_eq!(options.state_dir, Some(PathBuf::from("/tmp/s")));
        assert_eq!(options.planner.as_deref(), Some("rule"));
        assert_eq!(parse(&[]).unwrap().unwrap().mode, Mode::Serve);
        assert_eq!(
            parse(&["--verify-audit"]).unwrap().unwrap().mode,
            Mode::VerifyAudit
        );
        for args in [
            vec!["--plannr", "rule"],
            vec!["--state-dir"],
            vec!["--socket", "--planner"],
            vec!["serve"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn task_budgets_come_from_the_environment_and_a_typo_is_not_no_limit() {
        let from = |pairs: &'static [(&'static str, &'static str)]| {
            task_budget(move |name| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            })
        };
        assert_eq!(from(&[]).unwrap(), Budget::default());
        let set = from(&[
            ("AGENTOS_TASK_TOKENS", "5000"),
            ("AGENTOS_TASK_SECONDS", " 90 "),
        ])
        .unwrap();
        assert_eq!(set.tokens, Some(5000));
        assert_eq!(set.active, Some(std::time::Duration::from_secs(90)));
        let unlimited =
            from(&[("AGENTOS_TASK_TOKENS", "0"), ("AGENTOS_TASK_SECONDS", "0")]).unwrap();
        assert_eq!((unlimited.tokens, unlimited.active), (None, None));
        for bad in ["many", "-1", "1e6"] {
            let pairs: &'static [(&str, &str)] = Box::leak(
                vec![(
                    "AGENTOS_TASK_TOKENS",
                    &*Box::leak(bad.to_string().into_boxed_str()),
                )]
                .into_boxed_slice(),
            );
            assert!(from(pairs).is_err(), "{bad}");
        }
    }
}
