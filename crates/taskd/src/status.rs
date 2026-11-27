//! `system.status`: the task service's own account of how it is running and
//! what needs the user's attention, with what to do about each.
//!
//! Everything here is something the service already knew when it started
//! (planner, sandbox, PIN, a previous crash, which MCP servers came up), but
//! that used to live only in the journal. As a capability the agent can
//! answer "what is wrong with you" or "why does every plugin call ask me"
//! from the facts, and `agentos doctor` can show the same report. It changes
//! nothing: L0.
//!
//! The report is the service's own word and nothing else. What an MCP server
//! says about itself (its errors, its tool names) stays in the journal; the
//! report names the server as the user did in `mcp.json` and counts its
//! tools. That keeps the output trusted, so a client may read it directly
//! and an agent that reads it is not tainted.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use agentos_core::{Capability, CapabilityError, CapabilityName, CapabilitySpec, RiskLevel};
use agentos_ipc::status::{self, McpServer, Problem, Severity, SystemStatus};
use agentos_services::identity::PinVault;
use agentos_services::schedule::ScheduleStore;
use async_trait::async_trait;
use serde_json::{json, Value};

/// What went wrong or is switched off while the service started. Each one
/// becomes a problem in the report.
#[derive(Debug, Default, Clone)]
pub struct Findings {
    pub rule_planner: bool,
    pub sandboxed: bool,
    /// The audit log could not be continued and was moved here.
    pub audit_set_aside: Option<PathBuf>,
    /// First line of the previous run's crash report.
    pub crash: Option<String>,
    pub files_error: Option<String>,
    pub smart_home_error: Option<String>,
    pub mcp_config_error: Option<String>,
    /// Each enabled MCP server and how many tools it registered.
    pub mcp_servers: Vec<McpServer>,
    pub judge_without_model: bool,
    pub improve_without_model: bool,
    pub plugin_problems: Vec<String>,
}

/// The problems that follow from what was found at startup.
pub fn problems(found: &Findings, status: &SystemStatus) -> Vec<Problem> {
    let restart = "然后重启任务服务";
    let mut problems = Vec::new();
    if let Some(old) = &found.audit_set_aside {
        problems.push(
            Problem::new(
                Severity::Error,
                format!(
                    "启动时审计日志无法继续写入（已损坏或被改动过），原文件保留为 {}，已开始新的日志",
                    old.display()
                ),
            )
            .fix(format!(
                "运行 agentos-taskd --verify-audit --audit {} 看哪一行断开；保留这个文件，不要删除",
                old.display()
            )),
        );
    }
    if let Some(summary) = &found.crash {
        problems.push(
            Problem::new(
                Severity::Warning,
                format!("这次启动之前，任务服务异常退出过：{summary}"),
            )
            .fix(format!(
                "完整报告在 {}/crash.prev.txt，也在 journalctl -t agentos-taskd 里；可以附在 issue 里报告",
                status.state_dir
            )),
        );
    }
    if !found.sandboxed {
        problems.push(
            Problem::new(
                Severity::Warning,
                "没有可用的沙箱（bubblewrap）：插件每次调用都要你确认；MCP server 只有在 mcp.json 里写了 \"sandbox\": false 才会启动",
            )
            .fix(format!(
                "在 Linux 上安装 bubblewrap（例如 sudo apt install bubblewrap），确认 bwrap 能运行，{restart}；macOS 上没有这个沙箱"
            )),
        );
    }
    if let Some(error) = &found.mcp_config_error {
        problems.push(
            Problem::new(
                Severity::Warning,
                format!("mcp.json 读不出来，没有启动任何 MCP server：{error}"),
            )
            .fix(format!("修好状态目录里的 mcp.json，{restart}")),
        );
    }
    for server in found.mcp_servers.iter().filter(|server| server.tools == 0) {
        problems.push(
            Problem::new(
                Severity::Warning,
                format!("MCP server {} 没有启动，或者没有提供任何工具", server.name),
            )
            .fix("原因在任务服务的日志里（镜像里 journalctl -t agentos-taskd，自己运行的看它的输出）。常见的是：没有可用的沙箱又没写 \"sandbox\": false；命令装在主目录里（nvm 的 npx、uv 的 uvx）却没写进 sandbox.read；sandbox 规则写错"),
        );
    }
    if let Some(error) = &found.files_error {
        problems.push(
            Problem::new(
                Severity::Warning,
                format!("文件工作区不可用，没有 files.* 能力：{error}"),
            )
            .fix(format!(
                "把 AGENTOS_FILES_DIR 设成一个不在状态目录里的目录，{restart}"
            )),
        );
    }
    if let Some(error) = &found.smart_home_error {
        problems.push(
            Problem::new(
                Severity::Warning,
                format!("Home Assistant 没有接上：{error}"),
            )
            .fix(format!("检查 HASS_URL 和 HASS_TOKEN，{restart}")),
        );
    }
    let configure_model = format!(
        "设置 ANTHROPIC_API_KEY，或 OPENAI_MODEL 加 OPENAI_API_KEY / OPENAI_BASE_URL，{restart}"
    );
    if found.judge_without_model {
        problems.push(
            Problem::new(
                Severity::Warning,
                "设置了 AGENTOS_JUDGE=1，但没有配置模型，裁判没有启用",
            )
            .fix(configure_model.clone()),
        );
    }
    if found.improve_without_model {
        problems.push(
            Problem::new(
                Severity::Warning,
                "设置了 AGENTOS_IMPROVE=1，但没有模型规划器，自我改进没有启用",
            )
            .fix(configure_model.clone()),
        );
    }
    for problem in &found.plugin_problems {
        problems.push(
            Problem::new(Severity::Warning, problem.clone())
                .fix("说“看看插件”查看，“回滚 名字”退回上一个版本"),
        );
    }
    if found.rule_planner {
        problems.push(
            Problem::new(
                Severity::Info,
                "正在用离线规则规划器：只认识“记一下 …”“看看笔记”“现在几点”这类固定说法",
            )
            .fix(configure_model),
        );
    }
    problems
}

/// The PIN problem, if there is one. The vault follows the PIN file, so a
/// PIN set or cleared beside the service is in force at once; what remains
/// is no PIN at all, or a file the vault could not read, in which case it
/// keeps enforcing the last PIN it could.
fn pin_problem(set: bool, unreadable: Option<String>) -> Option<Problem> {
    // `--set-pin` reads the old file first and stops at a damaged one;
    // `--clear-pin` only removes it.
    let fix = "修好它，或者运行 agentos-taskd --clear-pin 再 agentos-taskd --set-pin 重新设置";
    match (set, unreadable) {
        (true, None) => None,
        (false, None) => Some(
            Problem::new(
                Severity::Warning,
                "没有设置 PIN：不可逆（L3）的操作一律拒绝",
            )
            .fix("运行 agentos-taskd --set-pin，不需要重启任务服务"),
        ),
        (true, Some(error)) => Some(
            Problem::new(
                Severity::Error,
                format!("PIN 文件读不出来（{error}），任务服务继续要求它最后读到的 PIN"),
            )
            .fix(fix),
        ),
        (false, Some(error)) => Some(
            Problem::new(
                Severity::Error,
                format!("PIN 文件读不出来（{error}），任务服务仍然当作没有 PIN：不可逆（L3）的操作一律拒绝"),
            )
            .fix(fix),
        ),
    }
}

/// The `system.status` capability. Registered early, like every capability,
/// and given its report once the service has finished starting.
pub struct StatusCapability {
    spec: CapabilitySpec,
    report: OnceLock<SystemStatus>,
    pins: Arc<PinVault>,
    schedules: Arc<ScheduleStore>,
}

impl StatusCapability {
    pub fn new(pins: Arc<PinVault>, schedules: Arc<ScheduleStore>) -> Self {
        Self {
            spec: CapabilitySpec {
                name: CapabilityName::new(status::CAPABILITY),
                description: "AgentOS 任务服务自己的状态：版本、规划器、沙箱、PIN、上次崩溃、MCP 和插件的启动情况，以及需要用户处理的问题和办法。用户问系统哪里不对、为什么某个操作被拒绝或总要确认时先读它".into(),
                risk: RiskLevel::ReadOnly,
                input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            },
            report: OnceLock::new(),
            pins,
            schedules,
        }
    }

    /// Set the startup report. Later calls are ignored: the service starts
    /// once.
    pub fn publish(&self, mut report: SystemStatus, found: &Findings) {
        report.problems = problems(found, &report);
        report.mcp = found.mcp_servers.clone();
        let _ = self.report.set(report);
    }

    /// The report as of now, if the service has finished starting.
    pub fn current(&self) -> Option<SystemStatus> {
        let mut report = self.report.get()?.clone();
        report.pin_set = self.pins.is_set();
        report.schedules = self.schedules.list().len();
        report
            .problems
            .extend(pin_problem(report.pin_set, self.pins.unreadable()));
        status::sort(&mut report.problems);
        Some(report)
    }
}

#[async_trait]
impl Capability for StatusCapability {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        if !args.as_object().is_some_and(serde_json::Map::is_empty) {
            return Err(CapabilityError::InvalidArgs(
                "system.status 不接受参数".into(),
            ));
        }
        let report = self
            .current()
            .ok_or_else(|| CapabilityError::Failed("任务服务还在启动".into()))?;
        let mut value = serde_json::to_value(&report)
            .map_err(|e| CapabilityError::Failed(format!("状态报告没有生成：{e}")))?;
        value["message"] = Value::String(report.render());
        Ok(value)
    }

    // `untrusted_output` stays false: see the module comment for what is
    // kept out of the report so that this is true.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capability(dir: &std::path::Path) -> StatusCapability {
        let pins = Arc::new(PinVault::open(dir.join("pin.json")).unwrap());
        let schedules = Arc::new(ScheduleStore::open(dir).unwrap());
        StatusCapability::new(pins, schedules)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agentos-status-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_machine_without_pin_or_sandbox_is_told_what_it_cannot_do_and_how_to_fix_it() {
        let dir = temp_dir("bare");
        let cap = capability(&dir);
        cap.publish(
            SystemStatus {
                version: "0.0.0".into(),
                state_dir: dir.display().to_string(),
                ..SystemStatus::default()
            },
            &Findings {
                rule_planner: true,
                crash: Some("agentos-taskd 0.0.0 崩溃：boom".into()),
                ..Findings::default()
            },
        );
        let value = cap.invoke(json!({})).await.unwrap();
        let report: SystemStatus = serde_json::from_value(value.clone()).unwrap();
        assert!(!report.pin_set);
        let texts: Vec<_> = report.problems.iter().map(|p| p.text.as_str()).collect();
        assert!(texts.iter().any(|t| t.contains("PIN")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("沙箱")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("boom")), "{texts:?}");
        // Every warning says what to do; nobody is left with a bare complaint.
        assert!(report
            .problems
            .iter()
            .filter(|p| p.severity >= Severity::Warning)
            .all(|p| p.fix.is_some()));
        // The offline planner is a fact, not a fault.
        assert_eq!(report.problems.last().unwrap().severity, Severity::Info);
        assert!(value["message"].as_str().unwrap().contains("怎么办"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_report_follows_a_pin_set_cleared_or_damaged_beside_the_service() {
        // Contract: the report says what the service enforces right now,
        // with no restart in between.
        let dir = temp_dir("pin");
        let cap = capability(&dir);
        cap.publish(
            SystemStatus::default(),
            &Findings {
                sandboxed: true,
                ..Findings::default()
            },
        );
        let before = cap.current().unwrap();
        assert!(!before.pin_set);
        assert!(before.problems[0].text.contains("没有设置 PIN"));

        // `agentos-taskd --set-pin` is another process with its own vault.
        PinVault::open(dir.join("pin.json"))
            .unwrap()
            .set("1234")
            .unwrap();
        let after = cap.current().unwrap();
        assert!(after.pin_set && after.problems.is_empty());

        // A damaged file is an error, and the last PIN stays in force.
        std::fs::write(dir.join("pin.json"), "{not json").unwrap();
        let damaged = cap.current().unwrap();
        assert!(damaged.pin_set);
        assert_eq!(damaged.problems[0].severity, Severity::Error);
        assert!(damaged.problems[0].text.contains("最后读到的 PIN"));

        // `--clear-pin` beside it: no PIN from now on.
        std::fs::remove_file(dir.join("pin.json")).unwrap();
        let cleared = cap.current().unwrap();
        assert!(!cleared.pin_set);
        assert!(cleared.problems[0].text.contains("没有设置 PIN"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_mcp_server_that_did_not_start_is_named_but_not_quoted() {
        // Contract: the report is trusted because no outside program's words
        // are in it; a server that failed is named as the user named it.
        let dir = temp_dir("mcp");
        let cap = capability(&dir);
        cap.publish(
            SystemStatus::default(),
            &Findings {
                sandboxed: true,
                mcp_servers: vec![
                    McpServer {
                        name: "fetch".into(),
                        tools: 0,
                    },
                    McpServer {
                        name: "git".into(),
                        tools: 3,
                    },
                ],
                ..Findings::default()
            },
        );
        assert!(!cap.untrusted_output());
        let report = cap.current().unwrap();
        let mcp: Vec<_> = report
            .problems
            .iter()
            .filter(|p| p.text.contains("MCP"))
            .collect();
        assert_eq!(mcp.len(), 1);
        assert!(
            mcp[0].text.contains("fetch") && mcp[0].fix.as_deref().unwrap().contains("journalctl")
        );
        assert!(report
            .render()
            .contains("MCP server：fetch（没有启动）、git（3 个工具）"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_report_is_read_only_and_takes_no_arguments() {
        let dir = temp_dir("args");
        let cap = capability(&dir);
        assert_eq!(cap.spec().risk, RiskLevel::ReadOnly);
        assert!(matches!(
            cap.invoke(json!({})).await,
            Err(CapabilityError::Failed(_))
        ));
        cap.publish(SystemStatus::default(), &Findings::default());
        assert!(matches!(
            cap.invoke(json!({"verbose": true})).await,
            Err(CapabilityError::InvalidArgs(_))
        ));
        let _ = std::fs::remove_dir_all(dir);
    }
}
