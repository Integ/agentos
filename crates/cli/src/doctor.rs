//! `agentos doctor`: one command that says whether AgentOS works on this
//! machine and, where it does not, what to do about it.
//!
//! It only reads. It pings the service and asks for its own report
//! (`system.status`); when the service cannot be reached it looks at what is
//! visible from here instead: the socket, and a crash report the service has
//! not picked up because it has not started again. Nothing is sent anywhere:
//! the report is for the user to read, or to attach to an issue.

use std::path::{Path, PathBuf};

use agentos_core::CapabilityName;
use agentos_ipc::status::{self, Problem, Severity, SystemStatus};
use agentos_ipc::{current_uid, Command, Reply};
use serde_json::{json, Value};

use crate::client::{Client, ClientError};

/// Where to read more, for someone who installed binaries without the repo.
pub const TROUBLESHOOTING: &str =
    "https://github.com/Integ/agentos/blob/main/docs/troubleshooting.md";

/// What `doctor` found.
#[derive(Debug)]
pub struct Report {
    pub client_version: String,
    pub socket: PathBuf,
    /// `None` when the service could not be reached.
    pub server_version: Option<String>,
    /// The service's report on itself; its problems are in `problems`.
    pub status: Option<SystemStatus>,
    /// Everything that needs attention, the service's and what was found
    /// from here, most severe first.
    pub problems: Vec<Problem>,
}

impl Report {
    pub fn connected(&self) -> bool {
        self.server_version.is_some()
    }

    /// 0 when nothing needs attention, 1 when the service could not be
    /// reached (as for any other command), 3 when it runs but something
    /// needs attention.
    pub fn exit_code(&self) -> i32 {
        if !self.connected() {
            return 1;
        }
        match self.problems.iter().map(|p| p.severity).max() {
            Some(Severity::Warning | Severity::Error) => 3,
            _ => 0,
        }
    }

    pub fn json(&self) -> Value {
        json!({
            "client_version": self.client_version,
            "socket": self.socket.display().to_string(),
            "connected": self.connected(),
            "server_version": self.server_version,
            "status": self.status,
            "problems": self.problems,
        })
    }

    pub fn text(&self) -> String {
        let mut lines = vec![format!("AgentOS 诊断 · agentos v{}", self.client_version)];
        lines.push(match &self.server_version {
            Some(version) => format!("连接：{}（任务服务 v{version}）", self.socket.display()),
            None => format!("连接：没有连上 {}", self.socket.display()),
        });
        if let Some(report) = &self.status {
            lines.push(report.render_facts());
        }
        lines.push(status::render_problems(&self.problems));
        if self
            .problems
            .iter()
            .any(|p| p.severity >= Severity::Warning)
        {
            lines.push(
                "重启任务服务：镜像里运行 systemctl --user restart agentos-taskd；自己运行的，停止后用同样的参数再启动。"
                    .into(),
            );
            lines.push(format!("更多排障办法：{TROUBLESHOOTING}"));
        }
        lines.push(
            "这份报告只在本机生成，没有发送到任何地方；贴到 issue 之前看一眼里面的路径和地址。"
                .into(),
        );
        lines.join("\n")
    }
}

/// Look at the service behind `client`. `state_dir` is where to look for a
/// crash report when the service is down.
pub async fn examine(client: &Client, state_dir: Option<&Path>) -> Report {
    let socket = client.path().to_path_buf();
    let mut problems = Vec::new();
    let server_version = match client.request(Command::Ping {}).await {
        Ok(Reply::Pong { server_version, .. }) => Some(server_version),
        // The client only accepts a pong for a ping.
        Ok(_) => None,
        Err(error) => {
            problems.extend(unreachable(&socket, &error, state_dir));
            None
        }
    };
    let mut status = None;
    if let Some(version) = &server_version {
        problems.extend(version_mismatch(env!("CARGO_PKG_VERSION"), version));
        let read = Command::Read {
            capability: CapabilityName::new(status::CAPABILITY),
            args: json!({}),
        };
        match client.request(read).await {
            Ok(Reply::Value { value, .. }) => match serde_json::from_value::<SystemStatus>(value) {
                Ok(mut report) => {
                    problems.append(&mut report.problems);
                    status = Some(report);
                }
                Err(error) => problems.push(Problem::new(
                    Severity::Warning,
                    format!("看不懂任务服务的状态报告：{error}"),
                )),
            },
            Ok(_) => {}
            Err(error) => problems.push(no_report(version, &error)),
        }
    }
    status::sort(&mut problems);
    Report {
        client_version: env!("CARGO_PKG_VERSION").into(),
        socket,
        server_version,
        status,
        problems,
    }
}

/// Why the service could not be reached, from what is visible here.
pub fn unreachable(socket: &Path, error: &ClientError, state_dir: Option<&Path>) -> Vec<Problem> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let start = "镜像里运行 systemctl --user start agentos-taskd，日志用 journalctl --user -u agentos-taskd 查看；自己运行的，用和 agentos 相同的状态目录启动 agentos-taskd --state-dir <目录>";
    let first = match std::fs::symlink_metadata(socket) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Problem::new(
            Severity::Error,
            format!("任务服务没有运行：{} 不存在", socket.display()),
        )
        .fix(start),
        Ok(meta)
            if !meta.file_type().is_socket()
                || meta.uid() != current_uid()
                || meta.mode() & 0o077 != 0 =>
        {
            Problem::new(Severity::Error, format!("连不上任务服务：{error}")).fix(
                "确认 --socket、--state-dir 或 AGENTOS_TASK_SOCKET 指向任务服务自己创建的 socket；它必须归你所有，只有你能访问",
            )
        }
        _ => Problem::new(Severity::Error, format!("连不上任务服务：{error}")).fix(format!(
            "socket 还在，任务服务可能已经退出或没有响应。{start}"
        )),
    };
    let mut problems = vec![first];
    if let Some(crash) = state_dir.and_then(pending_crash) {
        problems.push(crash);
    }
    problems
}

/// A crash report the service writes as it dies and moves aside when it
/// starts again; still being there means it has not started since.
fn pending_crash(state_dir: &Path) -> Option<Problem> {
    let path = state_dir.join("crash.txt");
    let text = std::fs::read_to_string(&path).ok()?;
    let first: String = text
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(300)
        .collect();
    Some(
        Problem::new(
            Severity::Error,
            format!("任务服务崩溃了，之后还没有重新启动：{first}"),
        )
        .fix(format!(
            "完整报告在 {}；镜像里用户服务会自动重启它，连续崩溃时间隔会拉长到 30 秒。可以把报告附在 issue 里",
            path.display()
        )),
    )
}

/// The terminal and the service come from different builds.
pub fn version_mismatch(client: &str, server: &str) -> Option<Problem> {
    (client != server).then(|| {
        Problem::new(
            Severity::Warning,
            format!("终端里的 agentos 是 v{client}，任务服务是 v{server}"),
        )
        .fix("更新后重启任务服务（镜像里重新登录一次），让两边是同一个版本")
    })
}

/// The service answered a ping but not the status read: older than
/// `system.status`, or something went wrong in between.
fn no_report(version: &str, error: &ClientError) -> Problem {
    Problem::new(
        Severity::Warning,
        format!("任务服务 v{version} 没有给出状态报告，只检查了连接：{error}"),
    )
    .fix("把任务服务更新到和 agentos 相同的版本，然后重启它")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agentos-doctor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn refused() -> ClientError {
        ClientError::Transport {
            message: "refused".into(),
            ambiguous: false,
        }
    }

    #[test]
    fn a_service_that_crashed_and_did_not_come_back_is_named_with_its_crash() {
        let dir = temp_dir("crash");
        std::fs::write(
            dir.join("crash.txt"),
            "agentos-taskd 0.4.0 崩溃：boom（位置 src/x.rs:7）\nbacktrace\n",
        )
        .unwrap();
        let problems = unreachable(&dir.join("control.sock"), &refused(), Some(&dir));
        assert_eq!(problems.len(), 2);
        assert!(problems[0].text.contains("没有运行"));
        assert!(problems[0]
            .fix
            .as_deref()
            .unwrap()
            .contains("agentos-taskd"));
        assert!(problems[1].text.contains("boom") && !problems[1].text.contains("backtrace"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_path_that_is_not_the_service_s_socket_is_not_called_a_stopped_service() {
        let dir = temp_dir("file");
        let path = dir.join("control.sock");
        std::fs::write(&path, "").unwrap();
        let problems = unreachable(&path, &refused(), None);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].fix.as_deref().unwrap().contains("归你所有"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_differing_versions_are_a_problem() {
        assert_eq!(version_mismatch("0.5.0", "0.5.0"), None);
        let problem = version_mismatch("0.5.0", "0.4.0").unwrap();
        assert!(problem.text.contains("v0.5.0") && problem.text.contains("v0.4.0"));
    }

    #[test]
    fn the_exit_code_separates_down_from_needs_attention_from_fine() {
        let report = |server: Option<&str>, problems: Vec<Problem>| Report {
            client_version: "0".into(),
            socket: PathBuf::from("/x"),
            server_version: server.map(Into::into),
            status: None,
            problems,
        };
        assert_eq!(report(None, vec![]).exit_code(), 1);
        assert_eq!(report(Some("0"), vec![]).exit_code(), 0);
        assert_eq!(
            report(Some("0"), vec![Problem::new(Severity::Info, "x")]).exit_code(),
            0
        );
        let attention = report(Some("0"), vec![Problem::new(Severity::Warning, "x")]);
        assert_eq!(attention.exit_code(), 3);
        assert!(attention.text().contains(TROUBLESHOOTING));
    }
}
