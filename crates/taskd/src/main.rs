//! AgentOS system service. Task ownership is independent of client lifetimes.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentos_audit::AuditLog;
use agentos_core::AgentId;
use agentos_ipc::status::SystemStatus;
use agentos_planner::claude::{ClaudeConfig, ClaudePlanner};
use agentos_planner::llm::TextModel;
use agentos_planner::openai::{OpenAiConfig, OpenAiPlanner};
use agentos_planner::{Planner, RulePlanner};
use agentos_policy::PolicyEngine;
use agentos_runtime::{TaskId, TaskStore};
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
use agentos_services::schedule::{self, ScheduleStore};
use agentos_services::{files, web};
use agentos_taskd::builtin;
use agentos_taskd::butler::{grant_manifest, Budget, Butler};
use agentos_taskd::callers::CallerStore;
use agentos_taskd::crash;
use agentos_taskd::improve::ImproveReport;
use agentos_taskd::learn::{Config as LearnConfig, Improver, LearnedList, Rollback};
use agentos_taskd::learned::{self, LearnedStore};
use agentos_taskd::migrate;
use agentos_taskd::preauth::PreauthStore;
use agentos_taskd::replay::{self, Pick, Replayer, Summary};
use agentos_taskd::service::Service;
use agentos_taskd::status::{Findings, StatusCapability};
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
  --replay                      把最近的任务重新交给现在的系统，和当时的结局比较，然后退出；
                                能力一律用当时记录的结果代替，不执行任何真实操作；
                                有任务以前成功、现在不成功时退出码为 1
    --limit N                   回放最近 N 个可以回放的任务（默认 20）
    --tasks task-1,task-5       只回放这些任务
                                回放默认用离线规则规划器；用模型要写明 --planner（会消耗 token）
  --help, -h                    查看帮助
  --version, -V                 查看版本

状态目录里的可选配置：mcp.json（MCP server）、agents/*.json（自定义子 Agent）。
环境变量：HASS_URL 与 HASS_TOKEN 接入 Home Assistant；AGENTOS_JUDGE=1 启用裁判；
AGENTOS_IMPROVE=1 在空闲时从历史里学习，评测明显变好就自动采用（需要模型，会消耗 token；
AGENTOS_IMPROVE_AUTO=0 只评测不采用）。

在另一终端运行 agentos workbench 连接工作台。
服务不读取 stdin；客户端退出后任务继续。SIGINT/SIGTERM 停止服务。";

#[derive(Default, Debug, PartialEq)]
enum Mode {
    #[default]
    Serve,
    SetPin,
    ClearPin,
    VerifyAudit,
    Replay,
}

#[derive(Default, Debug)]
struct Options {
    planner: Option<String>,
    audit: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    socket: Option<PathBuf>,
    mode: Mode,
    limit: Option<usize>,
    tasks: Vec<TaskId>,
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
            "--replay" => options.mode = Mode::Replay,
            "--planner" | "--audit" | "--state-dir" | "--socket" | "--limit" | "--tasks" => {
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
                    "--limit" => {
                        let limit = value.parse().ok().filter(|n| *n > 0).ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("--limit 需要一个正整数，不是 {value}"),
                            )
                        })?;
                        options.limit = Some(limit);
                    }
                    "--tasks" => {
                        for item in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                            let id = item.parse().map_err(|_| {
                                io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    format!("--tasks 里的 {item} 不是任务编号（例如 task-3）"),
                                )
                            })?;
                            options.tasks.push(id);
                        }
                    }
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
            "OpenAI 兼容规划器（{} @ {} · {}）",
            planner.model(),
            planner.base_url(),
            planner.api().label()
        );
        Ok((Box::new(planner), Some(Arc::new(text)), label))
    };
    // A mistyped OPENAI_API must not quietly pick the other format.
    if choice.as_deref() != Some("rule") && choice.as_deref() != Some("claude") {
        OpenAiConfig::api_from_env().map_err(|message| error(&message))?;
    }
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
    // A running service reads the PIN file at every check (see
    // identity.rs), so this is in force without a restart.
    println!(
        "PIN 已设置（{}）。不可逆的操作现在需要输入它才能批准，正在运行的任务服务也一样，不需要重启。",
        path.display()
    );
    Ok(())
}

fn clear_pin(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => println!(
            "已删除 PIN（{}）。不可逆的操作现在一律拒绝，正在运行的任务服务也一样。",
            path.display()
        ),
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

const DEFAULT_REPLAY_LIMIT: usize = 20;

/// Put recent tasks to the system as it is now and compare outcomes. It
/// reads the history without the daemon's lock and writes nothing of the
/// daemon's, so it may run beside a running service. The capabilities are
/// the ones that exist without the network or outside programs (built-ins,
/// the blackboard, plugins); a task that used anything else is skipped.
async fn replay(state_dir: &Path, planner: Option<String>, pick: Pick) -> io::Result<()> {
    // Model tokens cost money; a replay spends them only when asked to by
    // name, never because an API key happens to be in the environment.
    let choice = planner
        .or_else(|| std::env::var("AGENTOS_PLANNER").ok())
        .unwrap_or_else(|| "rule".into());
    let (planner, _, label) = make_planner(Some(choice))?;
    let tasks = state_file(
        "任务记录",
        &state_dir.join("tasks.json"),
        TaskStore::read_tasks(state_dir),
    )?;

    let mut registry = CapabilityRegistry::new();
    builtin::register_all(&mut registry);
    blackboard::register_all(&mut registry, Blackboard::in_memory());
    let plugins = Arc::new(PluginManager::open_read_only(
        state_dir.join("evolve"),
        SandboxMode::detect(),
    ));
    evolve::register_all(&mut registry, plugins.clone(), Arc::new(NoGenerator));
    for plugin in plugins.active() {
        // As in the service: a plugin never shadows a built-in.
        if registry.get(&plugin.spec().name).is_none() {
            registry.register(plugin);
        }
    }
    let capabilities = registry
        .specs()
        .into_iter()
        .filter_map(|spec| registry.get(&spec.name).cloned())
        .collect();
    let mut agents = AgentDirectory::with_builtin();
    for note in agents.load_dir(&state_dir.join("agents")) {
        eprintln!("{note}");
    }
    let budget = task_budget(|name| std::env::var(name).ok())
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    let scratch = std::env::temp_dir().join(format!("agentos-replay-{}", std::process::id()));
    agentos_runtime::create_private_dir(&scratch)?;
    // What the system learned is part of the system as it is now.
    let improve_dir = state_dir.join("improve");
    let learned = state_file(
        "学到的经验和技能",
        &improve_dir.join(learned::FILE),
        LearnedStore::open(&improve_dir),
    )?;
    let replayer = Replayer::new(
        capabilities,
        agents,
        budget,
        Arc::from(planner),
        scratch.clone(),
    )
    .with_learned((*learned.current().1).clone());

    let (cases, skipped) = replay::cases(&tasks, &|name| replayer.offers(name), &pick);
    eprintln!("用{label}回放 {} 个任务，不执行任何真实操作……", cases.len());
    let mut summary = Summary {
        cases: Vec::with_capacity(cases.len()),
        skipped,
    };
    let mut failure = None;
    for case in &cases {
        match replayer.run(case).await {
            Ok(result) => summary.cases.push(result),
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    if let Some(error) = failure {
        return Err(io::Error::new(
            error.kind(),
            format!("回放没有完成：{error}"),
        ));
    }
    println!("{}", replay::render(&summary, &label));
    if summary.regressed() {
        std::process::exit(1);
    }
    Ok(())
}

/// Name the state file a startup failure came from and what to do about
/// it. A damaged file is never repaired or replaced automatically (it may be
/// the only copy of the user's data), so the person reading the log, or
/// `systemctl --user status`, must learn which file it is and that moving it
/// aside lets the service start empty.
fn state_file<T>(what: &str, path: &Path, result: io::Result<T>) -> io::Result<T> {
    result.map_err(|error| {
        let message = if error.kind() == io::ErrorKind::InvalidData {
            format!(
                "{what}（{}）已损坏，读不出来：{error}。文件没有被改动；修好它，或者把它改名移走（例如加上 .bad 后缀）再启动，那样{what}会从空白开始",
                path.display()
            )
        } else {
            format!("打不开{what}（{}）：{error}", path.display())
        };
        io::Error::new(error.kind(), message)
    })
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match serve().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // Display, not Debug: this is the line the user reads.
            eprintln!("agentos-taskd 无法运行：{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn serve() -> io::Result<()> {
    let options = match options(std::env::args().skip(1))? {
        Ok(options) => options,
        Err(text) => {
            println!("{text}");
            return Ok(());
        }
    };
    // Only the desktop user's own service takes over agentosd's data; a
    // state directory named on the command line is someone's test or tool.
    let explicit_state_dir = options.state_dir.is_some();
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
    let pick = if options.tasks.is_empty() {
        Pick::Latest(options.limit.unwrap_or(DEFAULT_REPLAY_LIMIT))
    } else {
        Pick::Tasks(options.tasks.clone())
    };
    match options.mode {
        Mode::Serve => {}
        Mode::SetPin => {
            agentos_runtime::create_private_dir(&state_dir)?;
            return set_pin(&pin_path);
        }
        Mode::ClearPin => return clear_pin(&pin_path),
        Mode::VerifyAudit => return verify_audit(&audit_path),
        Mode::Replay => return replay(&state_dir, options.planner, pick).await,
    }
    // What starting up found, for `system.status`: the journal is not where
    // a user (or the agent) looks when something does not work.
    let mut found = Findings::default();
    let mut startup = Vec::new();
    if !explicit_state_dir {
        if let Some(legacy) = migrate::legacy_dir(|name| std::env::var(name).ok()) {
            // Before anything opens these files. A failure is reported and
            // tried again next start; it does not keep the service down.
            match migrate::from_agentosd(&legacy, &state_dir) {
                Ok(Some(report)) => {
                    eprintln!("已从 agentosd 迁移数据（{} 未改动）：", legacy.display());
                    for line in report.lines() {
                        eprintln!("  {line}");
                        startup.push(format!("从 agentosd 迁移：{line}"));
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    let line = format!(
                        "从 agentosd 迁移数据失败（{}），下次启动时重试：{error}",
                        legacy.display()
                    );
                    eprintln!("{line}");
                    startup.push(line);
                }
            }
        }
    }
    // From here on a panic is reported and ends the service; see crash.rs.
    crash::install(&state_dir);
    let planner_choice = options.planner.clone();
    let (planner, text_model, label) = make_planner(options.planner)?;
    // Only the offline planner comes without a text model.
    found.rule_planner = text_model.is_none();
    let socket = options
        .socket
        .unwrap_or_else(|| agentos_ipc::default_socket_path(&state_dir));
    let tasks = Arc::new(state_file(
        "任务记录",
        &state_dir.join("tasks.json"),
        TaskStore::open(&state_dir),
    )?);
    // A damaged or altered log must not keep the machine from starting: it
    // is set aside untouched and a new one begins, saying so.
    let (audit, set_aside) = AuditLog::open_or_set_aside(audit_path)?;
    if let Some(old) = &set_aside {
        eprintln!(
            "审计日志无法继续写入（已损坏或被改动），原文件保留为 {}，已开始新的日志。",
            old.display()
        );
    }
    found.audit_set_aside = set_aside;
    let audit = Arc::new(audit);
    if let Some(summary) = crash::take_last(&state_dir) {
        eprintln!(
            "上次 agentos-taskd 异常退出：{summary}\n  完整报告：{}（也在 journalctl -t agentos-taskd 里）",
            state_dir.join("crash.prev.txt").display()
        );
        found.crash = Some(summary.clone());
        audit.append(agentos_audit::AuditEvent::DaemonCrashed { summary })?;
    }
    let pins = Arc::new(
        state_file("PIN 文件", &pin_path, PinVault::open(&pin_path))?.with_audit(audit.clone()),
    );

    let mut registry = CapabilityRegistry::new();
    let notes_path = state_dir.join("notes.json");
    state_file(
        "笔记",
        &notes_path,
        builtin::register_persistent(&mut registry, &notes_path),
    )?;
    // Self-improvement: a read-only report on the system's own history,
    // and what it learned from it, with the user's undo.
    registry.register(Arc::new(ImproveReport::new(
        tasks.clone(),
        audit.path().to_path_buf(),
    )));
    let improve_dir = state_dir.join("improve");
    let learned = Arc::new(state_file(
        "学到的经验和技能",
        &improve_dir.join(learned::FILE),
        LearnedStore::open(&improve_dir),
    )?);
    registry.register(Arc::new(LearnedList::new(learned.clone())));
    registry.register(Arc::new(Rollback::new(learned.clone(), audit.clone())));
    let board_path = state_dir.join("blackboard.json");
    let board = state_file("黑板", &board_path, Blackboard::open(&board_path))?;
    blackboard::register_all(&mut registry, board.clone());
    // Public web pages (outside content: reading one taints the task).
    web::register_all(&mut registry);
    // One workspace directory the agent may read and write, not the home.
    let files_label = match files_workspace(&state_dir) {
        Ok(workspace) => {
            let label = workspace.root().display().to_string();
            files::register_all(&mut registry, Arc::new(workspace));
            label
        }
        Err(error) => {
            eprintln!("文件工作区不可用，没有注册 files.*：{error}");
            found.files_error = Some(error.to_string());
            format!("不可用（{error}）")
        }
    };
    // Intents the user wants submitted later; the service runs them.
    let schedules = Arc::new(state_file(
        "定时任务",
        &state_dir.join(schedule::FILE),
        ScheduleStore::open(&state_dir),
    )?);
    schedule::register_all(&mut registry, schedules.clone());
    // The service's account of itself; its report is filled in below, once
    // everything has started.
    let status = Arc::new(StatusCapability::new(pins.clone(), schedules.clone()));
    registry.register(status.clone());
    // The last crash report in full, and what it cut short if this run
    // follows it, for the agent to explain when the user asks.
    registry.register(Arc::new(crash::CrashReport::new(
        &state_dir,
        found
            .crash
            .is_some()
            .then(|| tasks.interrupted_on_open().to_vec()),
    )));

    // Self-evolution: plugins the system wrote for itself, sandboxed.
    let sandbox = SandboxMode::detect();
    let evolve_dir = state_dir.join("evolve");
    let plugins = Arc::new(state_file(
        "插件目录",
        &evolve_dir,
        PluginManager::open(&evolve_dir, sandbox),
    )?);
    // A damaged plugin does not stop the service, but it must not vanish
    // without a word either.
    found.plugin_problems = plugins.problems();
    for problem in &found.plugin_problems {
        eprintln!("{problem}");
    }
    found.sandboxed = sandbox == SandboxMode::Bubblewrap;
    let text_model = text_model.or_else(fallback_text_model);
    let generator: Arc<dyn PluginGenerator> = match text_model.clone() {
        Some(model) => Arc::new(LlmGenerator::new(model)),
        None => Arc::new(NoGenerator),
    };
    evolve::register_all(&mut registry, plugins.clone(), generator);

    // <state>/mcp.json: tools from MCP servers, each in its own sandbox. What
    // a server must never see: everything of AgentOS's own, wherever it is.
    let mut private = vec![socket.clone(), audit.path().to_path_buf()];
    private.extend(migrate::legacy_dir(|name| std::env::var(name).ok()));
    let containment = mcp::Containment::new(
        sandbox,
        std::env::var_os("HOME").map(PathBuf::from),
        &state_dir,
        &private,
    );
    match McpConfig::load(&state_dir.join("mcp.json")) {
        Ok(config) => {
            // The notes may quote a server (its errors, its tool names), so
            // they go to the journal only; the status report counts tools.
            for note in mcp::register_all(&mut registry, &config, &containment).await {
                eprintln!("{note}");
            }
            let specs = registry.specs();
            let mut names: Vec<_> = config
                .servers
                .iter()
                .filter(|(_, server)| !server.disabled)
                .map(|(name, _)| name.clone())
                .collect();
            names.sort();
            for name in names {
                let prefix = format!("mcp.{}.", mcp::sanitize(&name));
                let tools = specs
                    .iter()
                    .filter(|spec| spec.name.as_str().starts_with(&prefix))
                    .count();
                found
                    .mcp_servers
                    .push(agentos_ipc::status::McpServer { name, tools });
            }
        }
        Err(e) => {
            eprintln!("{e}");
            found.mcp_config_error = Some(e.to_string());
        }
    }
    // HASS_URL + HASS_TOKEN: the smart home, through Home Assistant.
    let home = match HassConfig::from_env().map(Hass::new) {
        Some(Ok(hass)) => {
            let label = format!("Home Assistant（{}）", hass.base_url());
            homeassistant::register_all(&mut registry, Arc::new(hass));
            label
        }
        Some(Err(e)) => {
            found.smart_home_error = Some(e.to_string());
            format!("无法创建 Home Assistant 客户端：{e}")
        }
        None => "未配置".into(),
    };

    let butler_id = AgentId::new("butler");
    let mut policy = PolicyEngine::new();
    let manifest: Vec<_> = registry.specs().into_iter().map(|s| s.name).collect();
    grant_manifest(&mut policy, &butler_id, &manifest);

    let mut agents = AgentDirectory::with_builtin();
    for note in agents.load_dir(&state_dir.join("agents")) {
        eprintln!("{note}");
        startup.push(note);
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
        startup.push(note);
    }
    let voice = Voice::new(voice_config, audit.clone());
    let mut expr = ExpressionHub::new(audit.clone());
    if voice.output_enabled() {
        expr.add_output(Arc::new(SpeechOutput::new(voice.clone())));
    }
    let preauth_path = state_dir.join("preauthorizations.json");
    let callers_path = state_dir.join("callers.json");
    let tasks_handle = tasks.clone();
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
    .with_preauthorizations(Arc::new(state_file(
        "预授权",
        &preauth_path,
        PreauthStore::open(&preauth_path),
    )?))
    .with_callers(Arc::new(state_file(
        "外部调用方",
        &callers_path,
        CallerStore::open(&callers_path),
    )?))
    .with_learned(learned.clone());
    // AGENTOS_JUDGE=1: a model reviews allowed L1+ steps before they run.
    let judge_on = std::env::var("AGENTOS_JUDGE").is_ok_and(|v| v == "1" || v == "on");
    let judge = match (&text_model, judge_on) {
        (Some(model), true) => {
            butler = butler.with_judge(Arc::new(LlmJudge::new(model.clone())));
            "已启用"
        }
        (None, true) => {
            found.judge_without_model = true;
            "需要配置一个模型，未启用"
        }
        _ => "未启用",
    };

    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let voice_label = voice_label(&voice.status());
    let butler = Arc::new(butler);
    let improve = match improve_settings(|name| std::env::var(name).ok())
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?
    {
        None => "未启用（AGENTOS_IMPROVE=1 开启，会消耗 token）".to_string(),
        Some(settings) => match (make_planner(planner_choice)?, text_model.clone()) {
            ((_, None, _), _) | (_, None) => {
                found.improve_without_model = true;
                "需要模型规划器，未启用".to_string()
            }
            ((planner, Some(_), planner_label), Some(model)) => {
                let label = format!(
                    "已启用，空闲 {} 秒后每 {} 秒一轮，{}",
                    settings.idle.as_secs(),
                    settings.interval.as_secs(),
                    if settings.config.auto {
                        "通过评测即自动生效"
                    } else {
                        "只评测不生效（AGENTOS_IMPROVE_AUTO=0）"
                    }
                );
                let improver = Improver::new(
                    tasks_handle.clone(),
                    audit.clone(),
                    learned.clone(),
                    butler.clone(),
                    model,
                    Arc::from(planner),
                    planner_label,
                    budget,
                    settings.config.clone(),
                    state_dir.join("improve").join("scratch"),
                );
                tokio::spawn(improve_loop(improver, tasks_handle.clone(), settings));
                label
            }
        },
    };
    let audit_label = audit.path().display().to_string();
    let server = Service::with_butler(butler, audit, Some(pins.clone()))
        .with_voice(voice)
        .with_schedules(schedules.clone())
        .bind(&socket)
        .await?;
    status.publish(
        SystemStatus {
            version: env!("CARGO_PKG_VERSION").into(),
            started_at_ms: agentos_core::now_ms(),
            state_dir: state_dir.display().to_string(),
            socket: server.path().display().to_string(),
            audit: audit_label,
            planner: label,
            sandbox: sandbox.label().into(),
            files: files_label,
            smart_home: home,
            voice: voice_label,
            judge: judge.into(),
            improve,
            budget: budget_label(budget),
            agents: agent_names,
            startup,
            ..SystemStatus::default()
        },
        &found,
    );
    // The same report `agentos doctor` and the agent read, so the journal
    // and the user never see two versions of the service's state.
    if let Some(report) = status.current() {
        println!("{}", report.render());
    }
    println!("后台服务已就绪。使用 agentos --socket <路径> workbench 连接。");
    server
        .run_until(async move {
            tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
        })
        .await
}

/// When and how the self-improvement cycle runs.
#[derive(Debug, Clone, PartialEq)]
struct ImproveSettings {
    config: LearnConfig,
    /// How long nothing of the user's may have happened before a cycle.
    idle: std::time::Duration,
    /// Time between cycles.
    interval: std::time::Duration,
}

/// `AGENTOS_IMPROVE=1` turns the cycle on; off by default, because every
/// cycle spends model tokens. `AGENTOS_IMPROVE_IDLE` and
/// `AGENTOS_IMPROVE_INTERVAL` are seconds.
fn improve_settings(
    var: impl Fn(&str) -> Option<String>,
) -> Result<Option<ImproveSettings>, String> {
    match var("AGENTOS_IMPROVE").as_deref().map(str::trim) {
        None | Some("") | Some("0") | Some("off") => return Ok(None),
        Some("1") | Some("on") => {}
        Some(other) => return Err(format!("AGENTOS_IMPROVE 只能是 1 或 0，不是 {other}")),
    }
    let seconds = |name: &str, default: u64| -> Result<std::time::Duration, String> {
        match var(name) {
            None => Ok(std::time::Duration::from_secs(default)),
            Some(value) => value
                .trim()
                .parse()
                .map(std::time::Duration::from_secs)
                .map_err(|_| format!("{name} 必须是秒数，不是 {value}")),
        }
    };
    Ok(Some(ImproveSettings {
        config: LearnConfig::from_env(&var)?,
        idle: seconds("AGENTOS_IMPROVE_IDLE", 600)?,
        interval: seconds("AGENTOS_IMPROVE_INTERVAL", 1800)?.max(std::time::Duration::from_secs(1)),
    }))
}

/// Run a cycle when there are no unfinished tasks and the activity has been
/// quiet for `idle`. The cycle itself stops when new work arrives.
async fn improve_loop(improver: Improver, tasks: Arc<TaskStore>, settings: ImproveSettings) {
    loop {
        tokio::time::sleep(settings.interval).await;
        let now = agentos_core::now_ms();
        let activity = tasks.activity();
        let idle = !activity.has_unfinished
            && now.saturating_sub(activity.latest_update_ms) >= settings.idle.as_millis() as u64;
        if !idle {
            continue;
        }
        match improver.cycle().await {
            Ok(outcome) => eprintln!("{}", outcome.describe()),
            Err(error) => eprintln!("自我改进这一轮出错，下次再试：{error}"),
        }
    }
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

/// What a task may consume, from the environment: `AGENTOS_TASK_TOKENS`,
/// `AGENTOS_TASK_SECONDS` and, for a single call, `AGENTOS_CALL_SECONDS`.
/// Unset keeps the default; `0` removes the limit. A value that is not a
/// number is an error rather than silently no limit.
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
    if let Some(seconds) = limit("AGENTOS_CALL_SECONDS")? {
        budget.call = seconds.map(std::time::Duration::from_secs);
    }
    Ok(budget)
}

/// The agent's file workspace: `AGENTOS_FILES_DIR`, else `~/AgentOS`. It
/// may not overlap AgentOS's own state, old or current.
fn files_workspace(state_dir: &Path) -> io::Result<files::Workspace> {
    let root = match std::env::var_os("AGENTOS_FILES_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("AgentOS"))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "没有 HOME，也没有设置 AGENTOS_FILES_DIR",
                )
            })?,
    };
    let mut forbidden = vec![state_dir.to_path_buf()];
    forbidden.extend(migrate::legacy_dir(|name| std::env::var(name).ok()));
    files::Workspace::open(&root, state_dir.join("files-trash"), &forbidden)
}

fn budget_label(budget: Budget) -> String {
    let limit = |value: Option<String>| value.unwrap_or_else(|| "不限".into());
    format!(
        "{} 轮 · token {} · 执行时长 {} · 单次调用 {}",
        budget.rounds,
        limit(budget.tokens.map(|tokens| tokens.to_string())),
        limit(budget.active.map(|time| format!("{} 秒", time.as_secs()))),
        limit(budget.call.map(|time| format!("{} 秒", time.as_secs()))),
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
        let replay = parse(&["--replay", "--tasks", "task-3, 7", "--limit", "5"])
            .unwrap()
            .unwrap();
        assert_eq!(replay.mode, Mode::Replay);
        assert_eq!(replay.tasks, [TaskId(3), TaskId(7)]);
        assert_eq!(replay.limit, Some(5));
        for args in [
            vec!["--plannr", "rule"],
            vec!["--state-dir"],
            vec!["--socket", "--planner"],
            vec!["serve"],
            vec!["--replay", "--limit", "0"],
            vec!["--replay", "--limit", "many"],
            vec!["--replay", "--tasks", "first"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn self_improvement_is_off_unless_asked_for_and_typos_are_errors() {
        let from = |pairs: &'static [(&'static str, &'static str)]| {
            improve_settings(move |name| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            })
        };
        // Off by default: every cycle spends model tokens.
        assert_eq!(from(&[]).unwrap(), None);
        assert_eq!(from(&[("AGENTOS_IMPROVE", "0")]).unwrap(), None);
        let on = from(&[
            ("AGENTOS_IMPROVE", "1"),
            ("AGENTOS_IMPROVE_IDLE", "5"),
            ("AGENTOS_IMPROVE_INTERVAL", "0"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(on.idle, std::time::Duration::from_secs(5));
        assert_eq!(on.interval, std::time::Duration::from_secs(1));
        assert!(on.config.auto);
        assert!(from(&[("AGENTOS_IMPROVE", "yes")]).is_err());
        assert!(from(&[("AGENTOS_IMPROVE", "1"), ("AGENTOS_IMPROVE_IDLE", "soon")]).is_err());
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
            ("AGENTOS_CALL_SECONDS", "60"),
        ])
        .unwrap();
        assert_eq!(set.tokens, Some(5000));
        assert_eq!(set.active, Some(std::time::Duration::from_secs(90)));
        assert_eq!(set.call, Some(std::time::Duration::from_secs(60)));
        let unlimited = from(&[
            ("AGENTOS_TASK_TOKENS", "0"),
            ("AGENTOS_TASK_SECONDS", "0"),
            ("AGENTOS_CALL_SECONDS", "0"),
        ])
        .unwrap();
        assert_eq!(
            (unlimited.tokens, unlimited.active, unlimited.call),
            (None, None, None)
        );
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
