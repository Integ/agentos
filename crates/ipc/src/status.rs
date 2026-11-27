//! What the `system.status` capability reports: the task service's own
//! account of how it is running and what needs the user's attention.
//!
//! It is a capability's value, read with the protocol's `read` like any
//! other, but clients interpret it (`agentos doctor` combines it with what it
//! can see locally), so its shape lives next to the protocol, in one place.
//! Fields are added with defaults and unknown ones are ignored: an older or
//! newer service still yields a report rather than an error.

use serde::{Deserialize, Serialize};

/// The capability's name.
pub const CAPABILITY: &str = "system.status";

/// How much a problem matters, in increasing order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Worth knowing; nothing is wrong.
    Info,
    /// Something the user probably expects is off or degraded.
    Warning,
    /// Something is broken.
    Error,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "提示",
            Self::Warning => "注意",
            Self::Error => "错误",
        }
    }
}

/// One thing that is not as it should be, and what to do about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub severity: Severity,
    /// What is wrong and what it means for the user.
    pub text: String,
    /// What to do, when there is something to do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Problem {
    pub fn new(severity: Severity, text: impl Into<String>) -> Self {
        Self {
            severity,
            text: text.into(),
            fix: None,
        }
    }

    pub fn fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }
}

/// An MCP server from the user's `mcp.json`, by the name the user gave it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServer {
    pub name: String,
    /// Tools registered as capabilities; 0 when it did not start.
    pub tools: usize,
}

/// The service's report on itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemStatus {
    /// The service's version.
    pub version: String,
    pub started_at_ms: u64,
    pub state_dir: String,
    pub socket: String,
    pub audit: String,
    /// Which planner plans tasks, with its model and endpoint.
    pub planner: String,
    /// Whether plugins and MCP servers run in a sandbox.
    pub sandbox: String,
    /// Whether the running service holds a PIN, so L3 steps can be approved.
    pub pin_set: bool,
    pub files: String,
    pub smart_home: String,
    pub voice: String,
    pub judge: String,
    pub improve: String,
    pub budget: String,
    pub agents: Vec<String>,
    pub schedules: usize,
    /// Each enabled server in `mcp.json` and how many tools it gave.
    pub mcp: Vec<McpServer>,
    /// What the service said while starting about custom agents, voice and
    /// migration. Never what an outside program said: the report is the
    /// service's own word and is read without a task to carry taint.
    pub startup: Vec<String>,
    /// Most severe first.
    pub problems: Vec<Problem>,
}

impl SystemStatus {
    /// The most severe problem, if there is any.
    pub fn worst(&self) -> Option<Severity> {
        self.problems.iter().map(|p| p.severity).max()
    }

    /// The report as text, one fact per line, problems last.
    pub fn render(&self) -> String {
        format!(
            "{}\n{}",
            self.render_facts(),
            render_problems(&self.problems)
        )
    }

    /// The facts without the problems, for a client that adds its own.
    pub fn render_facts(&self) -> String {
        let mut lines = vec![
            format!("AgentOS 任务服务 v{}", self.version),
            format!("规划器：{}", self.planner),
            format!("沙箱：{}", self.sandbox),
            format!(
                "PIN：{}",
                if self.pin_set {
                    "已设置"
                } else {
                    "未设置"
                }
            ),
            format!("定时任务：{} 个", self.schedules),
            format!("文件工作区：{}", self.files),
            format!("智能家居：{}", self.smart_home),
            format!("语音：{}", self.voice),
            format!("裁判：{}", self.judge),
            format!("自我改进：{}", self.improve),
            format!("每个任务的预算：{}", self.budget),
            format!("状态目录：{}", self.state_dir),
            format!("Unix socket：{}", self.socket),
            format!("审计日志：{}", self.audit),
        ];
        if !self.agents.is_empty() {
            lines.push(format!("子 Agent：{}", self.agents.join("、")));
        }
        if !self.mcp.is_empty() {
            let servers: Vec<_> = self
                .mcp
                .iter()
                .map(|server| match server.tools {
                    0 => format!("{}（没有启动）", server.name),
                    n => format!("{}（{n} 个工具）", server.name),
                })
                .collect();
            lines.push(format!("MCP server：{}", servers.join("、")));
        }
        if !self.startup.is_empty() {
            lines.push("启动记录：".into());
            lines.extend(self.startup.iter().map(|line| format!("  {line}")));
        }
        lines.join("\n")
    }
}

/// Problems as text, or a line saying there are none.
pub fn render_problems(problems: &[Problem]) -> String {
    render_problems_with(problems, |_, label| label.to_string())
}

/// The same, with each `[label]` passed through `decorate`: where a terminal
/// client adds color, without a second copy of the layout.
pub fn render_problems_with(
    problems: &[Problem],
    decorate: impl Fn(Severity, &str) -> String,
) -> String {
    if problems.is_empty() {
        return "没有发现需要处理的问题。".into();
    }
    let mut lines = vec![format!("需要留意的 {} 件事：", problems.len())];
    for problem in problems {
        let label = format!("[{}]", problem.severity.label());
        lines.push(format!(
            "  {} {}",
            decorate(problem.severity, &label),
            problem.text
        ));
        if let Some(fix) = &problem.fix {
            lines.push(format!("    怎么办：{fix}"));
        }
    }
    lines.join("\n")
}

/// Most severe first; equal ones keep their order.
pub fn sort(problems: &mut [Problem]) {
    problems.sort_by_key(|problem| std::cmp::Reverse(problem.severity));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_from_a_different_version_still_reads() {
        // Contract: clients ignore fields they do not know and fill in the
        // ones an older service does not send, so `doctor` never fails on a
        // version mismatch it is meant to report.
        let status: SystemStatus = serde_json::from_value(serde_json::json!({
            "version": "9.9.9",
            "pin_set": true,
            "something_new": {"a": 1},
            "problems": [{"severity": "warning", "text": "x"}],
        }))
        .unwrap();
        assert_eq!(status.version, "9.9.9");
        assert!(status.pin_set && status.startup.is_empty());
        assert_eq!(status.worst(), Some(Severity::Warning));
    }

    #[test]
    fn problems_are_listed_most_severe_first_with_what_to_do() {
        let mut problems = vec![
            Problem::new(Severity::Info, "a"),
            Problem::new(Severity::Error, "b").fix("do b"),
            Problem::new(Severity::Warning, "c"),
        ];
        sort(&mut problems);
        let order: Vec<_> = problems.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(order, ["b", "c", "a"]);
        let text = render_problems(&problems);
        assert!(text.contains("[错误] b\n    怎么办：do b"));
        assert_eq!(render_problems(&[]), "没有发现需要处理的问题。");
    }
}
