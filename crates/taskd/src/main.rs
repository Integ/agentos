//! AgentOS system service. Task ownership is independent of client lifetimes.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use agentos_audit::durable::AuditLog;
use agentos_core::AgentId;
use agentos_policy::PolicyEngine;
use agentos_runtime::TaskStore;
use agentos_taskd::builtin;
use agentos_taskd::butler::grant_manifest;
use agentos_taskd::claude_planner::{ClaudeConfig, ClaudePlanner};
use agentos_taskd::openai_planner::{OpenAiConfig, OpenAiPlanner};
use agentos_taskd::planner::{Planner, RulePlanner};
use agentos_taskd::registry::CapabilityRegistry;
use agentos_taskd::service::Service;

#[derive(Default)]
struct Options {
    planner: Option<String>,
    audit: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    socket: Option<PathBuf>,
}

fn options() -> io::Result<Option<Options>> {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(name) = args.next() {
        if name == "--help" || name == "-h" {
            println!("AgentOS v{} · 后台任务服务\n用法：agentos-taskd [--planner rule|claude|openai] [--state-dir 路径] [--socket 路径] [--audit 路径]\n\n在另一终端运行 agentos workbench 连接工作台。\n服务不读取 stdin；客户端退出后任务继续。SIGINT/SIGTERM 停止服务。", env!("CARGO_PKG_VERSION"));
            return Ok(None);
        }
        if !matches!(
            name.as_str(),
            "--planner" | "--audit" | "--state-dir" | "--socket"
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("未知参数：{name}，使用 --help 查看用法"),
            ));
        }
        let value = args
            .next()
            .filter(|s| !s.is_empty() && !s.starts_with("--"))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} 缺少值")))?;
        match name.as_str() {
            "--planner" => options.planner = Some(value),
            "--audit" => options.audit = Some(value.into()),
            "--state-dir" => options.state_dir = Some(value.into()),
            "--socket" => options.socket = Some(value.into()),
            _ => unreachable!(),
        }
    }
    Ok(Some(options))
}

fn rule() -> (Box<dyn Planner>, String) {
    (Box::new(RulePlanner), "离线规则规划器".into())
}

fn make_planner(choice: Option<String>) -> io::Result<(Box<dyn Planner>, String)> {
    let choice = choice.or_else(|| std::env::var("AGENTOS_PLANNER").ok());
    let error = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message);
    let claude = |cfg: ClaudeConfig| -> io::Result<(Box<dyn Planner>, String)> {
        let planner = ClaudePlanner::new(cfg).map_err(io::Error::other)?;
        let label = format!("Claude 规划器（{}）", planner.model());
        Ok((Box::new(planner), label))
    };
    let openai = |cfg: OpenAiConfig| -> io::Result<(Box<dyn Planner>, String)> {
        let planner = OpenAiPlanner::new(cfg).map_err(io::Error::other)?;
        let label = format!(
            "OpenAI 兼容规划器（{} @ {}）",
            planner.model(),
            planner.base_url()
        );
        Ok((Box::new(planner), label))
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

#[tokio::main]
async fn main() -> io::Result<()> {
    let Some(options) = options()? else {
        return Ok(());
    };
    let (planner, label) = make_planner(options.planner)?;
    let state_dir = options
        .state_dir
        .map_or_else(agentos_ipc::default_state_dir, Ok)?;
    let socket = options
        .socket
        .unwrap_or_else(|| agentos_ipc::default_socket_path(&state_dir));
    let tasks = Arc::new(TaskStore::open(&state_dir)?);
    let audit_path = options
        .audit
        .or_else(|| {
            std::env::var_os("AGENTOS_TASK_AUDIT")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| state_dir.join("audit.jsonl"));
    let audit = Arc::new(AuditLog::open(audit_path)?);
    let mut registry = CapabilityRegistry::new();
    builtin::register_persistent(&mut registry, state_dir.join("notes.json"))?;
    let mut policy = PolicyEngine::new();
    let manifest: Vec<_> = registry.specs().into_iter().map(|s| s.name).collect();
    grant_manifest(&mut policy, &AgentId::new("butler"), &manifest);
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let server = Service::new(tasks, audit, registry, policy, planner)
        .bind(&socket)
        .await?;
    println!("AgentOS v{} · {label}\n状态目录：{}\nUnix socket：{}\n后台服务已就绪。使用 agentos --socket <路径> workbench 连接。", env!("CARGO_PKG_VERSION"), state_dir.display(), server.path().display());
    server
        .run_until(async move {
            tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
        })
        .await
}
