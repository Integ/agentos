use std::path::PathBuf;

use agentos_ipc::{default_socket_path, default_state_dir, Command, MAX_PAGE_SIZE};
use agentos_runtime::{TaskId, TaskState};

pub const HELP: &str = "AgentOS · 独立任务工作台\n\n用法：agentos [--socket 路径 | --state-dir 路径] [--json] <命令>\n\n  workbench                         实时终端工作台（默认）\n  submit <意图>                     提交任务，客户端退出后仍继续\n  tasks [--before task-N] [--limit N] [--state 状态]\n  task task-N                       查看计划、结果与批准令牌\n  cancel task-N                     取消任务的后续步骤\n  approve task-N 步骤 --confirmation 令牌\n  capabilities                      查看能力与风险\n  watch [--after revision]           持续接收变更/重新同步通知\n  --help                            查看帮助\n  --version                         查看版本\n\n--json 应放在命令前，输出结构化 JSON。批准步骤从 1 开始。\n工作台 /quit 只关闭客户端，不停止后台服务。";

pub const WORKBENCH_HELP: &str = "普通输入：提交意图；任务在后台继续\n/tasks 或 /refresh：刷新当前页　/next：更早任务　/prev：返回上一页\n/latest：最近任务　/task task-N：选中并展示完整计划与结果\n/cancel task-N：取消后续步骤　/approve task-N 步骤：批准当前已展示的确认请求\n/capabilities：查看能力　/help：帮助　/quit：退出工作台";

#[derive(Debug)]
pub struct Options {
    pub socket: PathBuf,
    pub json: bool,
    pub action: Action,
}

#[derive(Debug)]
pub enum Action {
    Request(Command),
    Watch(Option<u64>),
    Workbench,
    Help,
    Version,
}

fn task_id(text: &str) -> Result<TaskId, String> {
    text.parse::<TaskId>()
        .ok()
        .filter(|id| id.0 > 0)
        .ok_or_else(|| "任务编号应为 task-1 或正整数 1".into())
}

fn state(text: &str) -> Result<TaskState, String> {
    serde_json::from_value(serde_json::Value::String(text.into())).map_err(|_| {
        "状态应为 queued/planning/running/awaiting_confirmation/succeeded/failed/cancelled/interrupted".into()
    })
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let args: Vec<_> = args.into_iter().collect();
    let mut i = 0;
    let mut socket = None;
    let mut directory = None;
    let mut json = false;
    while let Some(arg) = args.get(i) {
        match arg.as_str() {
            "--socket" | "--state-dir" => {
                let value = args
                    .get(i + 1)
                    .filter(|v| !v.is_empty() && !v.starts_with("--"))
                    .ok_or_else(|| format!("{arg} 缺少路径"))?;
                if arg == "--socket" {
                    socket = Some(PathBuf::from(value));
                } else {
                    directory = Some(PathBuf::from(value));
                }
                i += 2;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            "--help" | "-h" => {
                return Ok(Options {
                    socket: PathBuf::new(),
                    json,
                    action: Action::Help,
                })
            }
            "--version" | "-V" => {
                return Ok(Options {
                    socket: PathBuf::new(),
                    json,
                    action: Action::Version,
                })
            }
            _ => break,
        }
    }
    if socket.is_some() && directory.is_some() {
        return Err("--socket 与 --state-dir 请只指定一个".into());
    }
    let socket = match socket {
        Some(path) => path,
        None => default_socket_path(&match directory {
            Some(path) => path,
            None => default_state_dir().map_err(|e| e.to_string())?,
        }),
    };
    let rest = &args[i..];
    let action = match rest.first().map(String::as_str) {
        None => Action::Workbench,
        Some("workbench") if rest.len() == 1 => Action::Workbench,
        Some("submit") if rest.len() > 1 => {
            let text = rest[1..].join(" ");
            if text.trim().is_empty() {
                return Err("提交意图不能为空".into());
            }
            Action::Request(Command::Submit { text })
        }
        Some("tasks") => {
            let mut before = None;
            let mut limit = 20;
            let mut filter = None;
            let mut index = 1;
            while index < rest.len() {
                let value = rest
                    .get(index + 1)
                    .ok_or_else(|| format!("{} 缺少值", rest[index]))?;
                match rest[index].as_str() {
                    "--before" => before = Some(task_id(value)?),
                    "--limit" => {
                        limit = value
                            .parse::<usize>()
                            .ok()
                            .filter(|n| (1..=MAX_PAGE_SIZE).contains(n))
                            .ok_or_else(|| format!("每页数量必须为 1–{MAX_PAGE_SIZE}"))?
                    }
                    "--state" => filter = Some(state(value)?),
                    _ => return Err(format!("未知 tasks 参数：{}", rest[index])),
                }
                index += 2;
            }
            Action::Request(Command::List {
                before,
                limit,
                state: filter,
            })
        }
        Some("task") if rest.len() == 2 => Action::Request(Command::Get {
            id: task_id(&rest[1])?,
        }),
        Some("cancel") if rest.len() == 2 => Action::Request(Command::Cancel {
            id: task_id(&rest[1])?,
        }),
        Some("approve") if rest.len() == 5 && rest[3] == "--confirmation" => {
            let step = rest[2]
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or("步骤必须是从 1 开始的正整数")?;
            if rest[4].is_empty() {
                return Err("批准令牌不能为空".into());
            }
            Action::Request(Command::Approve {
                id: task_id(&rest[1])?,
                step,
                confirmation: rest[4].clone(),
            })
        }
        Some("capabilities") if rest.len() == 1 => Action::Request(Command::Capabilities {}),
        Some("ping") if rest.len() == 1 => Action::Request(Command::Ping {}),
        Some("watch") if rest.len() == 1 => Action::Watch(None),
        Some("watch") if rest.len() == 3 && rest[1] == "--after" => Action::Watch(Some(
            rest[2].parse().map_err(|_| "revision 必须是非负整数")?,
        )),
        Some("help") if rest.len() == 1 => Action::Help,
        _ => return Err("未知命令或参数错误，使用 agentos --help 查看用法".into()),
    };
    if json && matches!(action, Action::Workbench) {
        return Err("--json 用于单次命令或 watch；工作台使用交互文本".into());
    }
    Ok(Options {
        socket,
        json,
        action,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum WorkbenchCommand {
    Submit(String),
    Refresh,
    Next,
    Prev,
    Latest,
    Task(TaskId),
    Cancel(TaskId),
    Approve { id: TaskId, step: usize },
    Capabilities,
    Help,
    Quit,
}

pub fn parse_workbench(text: &str) -> Result<WorkbenchCommand, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(WorkbenchCommand::Refresh);
    }
    if !text.starts_with('/') {
        return Ok(WorkbenchCommand::Submit(text.into()));
    }
    let words: Vec<_> = text.split_whitespace().collect();
    Ok(match words.as_slice() {
        ["/tasks"] | ["/refresh"] => WorkbenchCommand::Refresh,
        ["/next"] => WorkbenchCommand::Next,
        ["/prev"] => WorkbenchCommand::Prev,
        ["/latest"] => WorkbenchCommand::Latest,
        ["/task", id] => WorkbenchCommand::Task(task_id(id)?),
        ["/cancel", id] => WorkbenchCommand::Cancel(task_id(id)?),
        ["/approve", id, step] => WorkbenchCommand::Approve {
            id: task_id(id)?,
            step: step
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or("步骤必须从 1 开始")?,
        },
        ["/capabilities"] => WorkbenchCommand::Capabilities,
        ["/help"] => WorkbenchCommand::Help,
        ["/quit"] | ["/exit"] => WorkbenchCommand::Quit,
        _ => return Err("未知工作台命令，输入 /help 查看用法".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn argv(args: &[&str]) -> Result<Options, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn approval_requires_an_explicit_token_and_one_based_step() {
        for args in [
            vec!["approve", "task-1", "1"],
            vec!["approve", "task-1", "0", "--confirmation", "nonce"],
            vec!["approve", "task-1", "1", "--confirmation", ""],
        ] {
            assert!(argv(&args).is_err());
        }
        let Action::Request(Command::Approve {
            id,
            step,
            confirmation,
        }) = argv(&["approve", "task-3", "2", "--confirmation", "nonce"])
            .unwrap()
            .action
        else {
            panic!("not approval")
        };
        assert_eq!((id, step, confirmation), (TaskId(3), 2, "nonce".into()));
    }

    #[test]
    fn invalid_slash_commands_never_become_task_submissions() {
        for input in [
            "/approve task-1",
            "/approve task-1 0",
            "/cancel",
            "/tasks extra",
            "/unknown",
        ] {
            assert!(parse_workbench(input).is_err(), "{input}");
        }
        assert_eq!(
            parse_workbench("记一下 /cancel task-1"),
            Ok(WorkbenchCommand::Submit("记一下 /cancel task-1".into()))
        );
        assert_eq!(
            parse_workbench("y"),
            Ok(WorkbenchCommand::Submit("y".into()))
        );
    }

    #[test]
    fn pages_filters_and_global_options_are_validated() {
        let options = argv(&[
            "--socket",
            "/tmp/control.sock",
            "--json",
            "tasks",
            "--before",
            "task-9",
            "--limit",
            "5",
            "--state",
            "running",
        ])
        .unwrap();
        assert!(options.json);
        let Action::Request(Command::List {
            before,
            limit,
            state,
        }) = options.action
        else {
            panic!("not list")
        };
        assert_eq!(
            (before, limit, state),
            (Some(TaskId(9)), 5, Some(TaskState::Running))
        );
        for args in [
            vec!["tasks", "--limit", "0"],
            vec!["tasks", "--limit", "51"],
            vec!["tasks", "--state", "typo"],
            vec!["--socket", "a", "--state-dir", "b", "tasks"],
            vec!["task", "task-0"],
        ] {
            assert!(argv(&args).is_err());
        }
    }
}
