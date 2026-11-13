use std::path::PathBuf;
use std::time::Duration;

use agentos_core::CapabilityName;
use agentos_ipc::{
    default_socket_path, default_state_dir, ArgRule, Command, Matcher, MAX_CAPABILITY_NAME_BYTES,
    MAX_PAGE_SIZE,
};
use agentos_runtime::{TaskId, TaskState};

/// How long `submit --wait` waits when `--timeout` is not given.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(300);
const MAX_WAIT_SECONDS: u64 = 86_400;

pub const HELP: &str = "AgentOS · 独立任务工作台\n\n用法：agentos [--socket 路径 | --state-dir 路径] [--json] <命令>\n\n  workbench                         实时终端工作台（默认）\n  submit [--thread task-N] [--wait] [--timeout 秒] <意图>\n                                    提交任务，客户端退出后仍继续；--wait 等到任务结束再返回；\n                                    --thread 接着某个线程说（用它第一个任务的编号）\n  tasks [--before task-N] [--limit N] [--state 状态] [--thread task-N]\n  task task-N                       查看计划、结果与批准令牌\n  cancel task-N                     取消任务的后续步骤\n  reply task-N 步骤 <回答>          回答任务在这一步提出的问题\n  approve task-N 步骤 --confirmation 令牌 [--pin-stdin]\n                                    批准一步；不可逆的步骤加 --pin-stdin，从标准输入读取 PIN\n  preauthorize <能力> [规则…] [--any-other-args] [--expires 时长] [--pin-stdin]\n                                    预授权：符合规则的调用不再逐次确认。规则：\n                                    --equals 字段=值　--one-of 字段=值1,值2　--prefix 字段=文本\n                                    --under 字段=目录　--any 字段；时长如 30m、2h、7d\n  preauthorizations                 查看仍然有效的预授权\n  revoke pre-N                      撤销一条预授权，立即生效\n  capabilities                      查看能力与风险\n  read <能力> [JSON 参数]           直接读取只读（L0）能力，不创建任务\n  watch [--after revision]           持续接收变更/重新同步通知\n  --help                            查看帮助\n  --version                         查看版本\n\n--json 应放在命令前，输出结构化 JSON。批准步骤从 1 开始。\n工作台 /quit 只关闭客户端，不停止后台服务。\n\n退出码：0 成功；1 连接、协议或后台错误；2 用法错误。\nsubmit --wait 另有：3 任务结束但未成功（未能处理、失败、已取消或已中断）；\n4 任务在等待批准；5 等待超时（任务仍在后台继续）；6 任务在等待回答。";

pub const WORKBENCH_HELP: &str = "普通输入：提交意图，接着本次对话说；任务在后台继续\n/new：开始一段新的对话\n/tasks 或 /refresh：刷新当前页　/next：更早任务　/prev：返回上一页\n/latest：最近任务　/task task-N：选中并展示完整计划与结果\n/cancel task-N：取消后续步骤　/approve task-N 步骤：批准当前已展示的确认请求\n/reply task-N 步骤 回答：回答任务提出的问题\n/capabilities：查看能力　/help：帮助　/quit：退出工作台";

#[derive(Debug)]
pub struct Options {
    pub socket: PathBuf,
    pub json: bool,
    pub action: Action,
    /// Read the PIN for an irreversible step from standard input.
    pub pin_stdin: bool,
}

#[derive(Debug)]
pub enum Action {
    Request(Command),
    /// Submit, then wait until the task settles, needs approval, or `timeout`.
    SubmitAndWait {
        text: String,
        thread: Option<TaskId>,
        timeout: Duration,
    },
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

fn field_name(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() || text.contains('=') {
        return Err("参数规则需要字段名".into());
    }
    Ok(text.into())
}

/// A rule's value as typed: JSON when it parses as JSON (`3`, `true`,
/// `"3"`), otherwise the text itself, so `to=me@example.com` needs no quotes.
fn argument_value(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.into()))
}

/// `90`, `90s`, `30m`, `2h` or `7d`, in seconds.
fn duration_seconds(text: &str) -> Result<u64, String> {
    let (digits, unit) = match text.char_indices().last() {
        Some((at, unit)) if unit.is_ascii_alphabetic() => (&text[..at], unit),
        _ => (text, 's'),
    };
    let scale = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        _ => return Err("时长的单位是 s、m、h 或 d，例如 30m".into()),
    };
    digits
        .parse::<u64>()
        .ok()
        .filter(|count| *count > 0)
        .and_then(|count| count.checked_mul(scale))
        .ok_or_else(|| "时长应为正整数加单位，例如 30m".into())
}

fn state(text: &str) -> Result<TaskState, String> {
    serde_json::from_value(serde_json::Value::String(text.into())).map_err(|_| {
        "状态应为 queued/planning/running/awaiting_confirmation/awaiting_input/succeeded/unhandled/failed/cancelled/interrupted".into()
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
                    pin_stdin: false,
                })
            }
            "--version" | "-V" => {
                return Ok(Options {
                    socket: PathBuf::new(),
                    json,
                    action: Action::Version,
                    pin_stdin: false,
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
    let mut pin_stdin = false;
    let action = match rest.first().map(String::as_str) {
        None => Action::Workbench,
        Some("workbench") if rest.len() == 1 => Action::Workbench,
        Some("submit") if rest.len() > 1 => {
            // Options are only recognised before the intent, so the intent's
            // own words are never mistaken for flags. `--` ends the options.
            let mut wait = false;
            let mut timeout = None;
            let mut thread = None;
            let mut index = 1;
            while let Some(word) = rest.get(index) {
                match word.as_str() {
                    "--wait" => {
                        wait = true;
                        index += 1;
                    }
                    "--thread" => {
                        thread = Some(task_id(
                            rest.get(index + 1).ok_or("--thread 需要线程编号")?,
                        )?);
                        index += 2;
                    }
                    "--timeout" => {
                        let seconds = rest
                            .get(index + 1)
                            .and_then(|value| value.parse::<u64>().ok())
                            .filter(|seconds| (1..=MAX_WAIT_SECONDS).contains(seconds))
                            .ok_or_else(|| {
                                format!("--timeout 需要 1–{MAX_WAIT_SECONDS} 之间的秒数")
                            })?;
                        timeout = Some(Duration::from_secs(seconds));
                        index += 2;
                    }
                    "--" => {
                        index += 1;
                        break;
                    }
                    _ => break,
                }
            }
            if timeout.is_some() && !wait {
                return Err("--timeout 需要与 --wait 一起使用".into());
            }
            let text = rest[index..].join(" ");
            if text.trim().is_empty() {
                return Err("提交意图不能为空".into());
            }
            if wait {
                Action::SubmitAndWait {
                    text,
                    thread,
                    timeout: timeout.unwrap_or(DEFAULT_WAIT),
                }
            } else {
                Action::Request(Command::Submit { text, thread })
            }
        }
        Some("tasks") => {
            let mut before = None;
            let mut limit = 20;
            let mut filter = None;
            let mut thread = None;
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
                    "--thread" => thread = Some(task_id(value)?),
                    _ => return Err(format!("未知 tasks 参数：{}", rest[index])),
                }
                index += 2;
            }
            Action::Request(Command::List {
                before,
                limit,
                state: filter,
                thread,
            })
        }
        Some("task") if rest.len() == 2 => Action::Request(Command::Get {
            id: task_id(&rest[1])?,
        }),
        Some("cancel") if rest.len() == 2 => Action::Request(Command::Cancel {
            id: task_id(&rest[1])?,
        }),
        Some("reply") if rest.len() > 3 => {
            let step = rest[2]
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or("步骤必须是从 1 开始的正整数")?;
            let text = rest[3..].join(" ");
            if text.trim().is_empty() {
                return Err("回答不能为空".into());
            }
            Action::Request(Command::Reply {
                id: task_id(&rest[1])?,
                step,
                text,
            })
        }
        Some("approve")
            if (rest.len() == 5 || (rest.len() == 6 && rest[5] == "--pin-stdin"))
                && rest[3] == "--confirmation" =>
        {
            pin_stdin = rest.len() == 6;
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
                // Filled in from standard input, never from the command line,
                // where it would end up in shell history and process lists.
                pin: None,
            })
        }
        Some("preauthorize") if rest.len() > 1 => {
            let capability = rest[1].trim();
            if capability.is_empty()
                || capability.len() > MAX_CAPABILITY_NAME_BYTES
                || capability.starts_with("--")
            {
                return Err("preauthorize 需要能力名称，例如 mail.send".into());
            }
            let mut args = Vec::new();
            let mut other_args = false;
            let mut expires_in_seconds = None;
            let mut index = 2;
            while let Some(word) = rest.get(index) {
                let mut value = || {
                    index += 1;
                    rest.get(index)
                        .map(String::as_str)
                        .ok_or_else(|| format!("{word} 缺少值"))
                };
                match word.as_str() {
                    "--any-other-args" => other_args = true,
                    "--pin-stdin" => pin_stdin = true,
                    "--expires" => expires_in_seconds = Some(duration_seconds(value()?)?),
                    "--any" => args.push(ArgRule {
                        field: field_name(value()?)?,
                        matcher: Matcher::Any,
                    }),
                    "--equals" | "--one-of" | "--prefix" | "--under" => {
                        let (field, text) = value()?
                            .split_once('=')
                            .ok_or_else(|| format!("{word} 的写法是 字段=值"))?;
                        let matcher = match word.as_str() {
                            "--equals" => Matcher::Equals(argument_value(text)),
                            "--one-of" => {
                                Matcher::OneOf(text.split(',').map(argument_value).collect())
                            }
                            "--prefix" => Matcher::Prefix(text.into()),
                            _ => Matcher::Under(text.into()),
                        };
                        args.push(ArgRule {
                            field: field_name(field)?,
                            matcher,
                        });
                    }
                    _ => return Err(format!("未知 preauthorize 参数：{word}")),
                }
                index += 1;
            }
            Action::Request(Command::Preauthorize {
                capability: CapabilityName::new(capability),
                args,
                other_args,
                expires_in_seconds,
                // Filled in from standard input, like the PIN of an approval.
                pin: None,
            })
        }
        Some("preauthorizations") if rest.len() == 1 => {
            Action::Request(Command::Preauthorizations {})
        }
        Some("revoke") if rest.len() == 2 && rest[1].starts_with("pre-") => {
            Action::Request(Command::RevokePreauthorization {
                id: rest[1].clone(),
            })
        }
        Some("capabilities") if rest.len() == 1 => Action::Request(Command::Capabilities {}),
        Some("read") if rest.len() == 2 || rest.len() == 3 => {
            let name = rest[1].trim();
            if name.is_empty() || name.len() > MAX_CAPABILITY_NAME_BYTES || name.starts_with("--") {
                return Err("read 需要能力名称，例如 notes.list".into());
            }
            let args = match rest.get(2) {
                Some(text) => serde_json::from_str::<serde_json::Value>(text)
                    .ok()
                    .filter(serde_json::Value::is_object)
                    .ok_or("能力参数必须是 JSON 对象，例如 '{}'")?,
                None => serde_json::json!({}),
            };
            Action::Request(Command::Read {
                capability: CapabilityName::new(name),
                args,
            })
        }
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
        pin_stdin,
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
    Approve {
        id: TaskId,
        step: usize,
    },
    Reply {
        id: TaskId,
        step: usize,
        text: String,
    },
    /// Later intents start a new thread.
    New,
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
        ["/reply", id, step, answer, ..] => WorkbenchCommand::Reply {
            id: task_id(id)?,
            step: step
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or("步骤必须从 1 开始")?,
            // Everything after the step, as typed.
            text: text
                .splitn(4, char::is_whitespace)
                .nth(3)
                .map_or_else(|| answer.to_string(), |rest| rest.trim().to_string()),
        },
        ["/new"] => WorkbenchCommand::New,
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
            pin,
        }) = argv(&["approve", "task-3", "2", "--confirmation", "nonce"])
            .unwrap()
            .action
        else {
            panic!("not approval")
        };
        assert_eq!((id, step, confirmation), (TaskId(3), 2, "nonce".into()));
        assert!(pin.is_none());
        // A PIN is only ever read from standard input, never from an argument.
        let with_pin = argv(&[
            "approve",
            "task-3",
            "2",
            "--confirmation",
            "nonce",
            "--pin-stdin",
        ])
        .unwrap();
        assert!(with_pin.pin_stdin);
        assert!(!argv(&["tasks"]).unwrap().pin_stdin);
        for args in [
            vec![
                "approve",
                "task-3",
                "2",
                "--confirmation",
                "nonce",
                "--pin",
                "1234",
            ],
            vec!["approve", "task-3", "2", "--confirmation", "nonce", "1234"],
        ] {
            assert!(argv(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn waiting_options_are_only_read_before_the_intent() {
        let Action::SubmitAndWait { text, timeout, .. } =
            argv(&["submit", "--wait", "记一下", "买牛奶"])
                .unwrap()
                .action
        else {
            panic!("not a waiting submission")
        };
        assert_eq!((text.as_str(), timeout), ("记一下 买牛奶", DEFAULT_WAIT));
        let Action::SubmitAndWait { text, timeout, .. } =
            argv(&["submit", "--timeout", "9", "--wait", "现在几点"])
                .unwrap()
                .action
        else {
            panic!("not a waiting submission")
        };
        assert_eq!(
            (text.as_str(), timeout),
            ("现在几点", Duration::from_secs(9))
        );
        // Words after the intent starts, or after `--`, stay part of the intent.
        for (args, expected) in [
            (
                vec!["submit", "记一下", "--wait", "的用法"],
                "记一下 --wait 的用法",
            ),
            (vec!["submit", "--", "--wait"], "--wait"),
        ] {
            let Action::Request(Command::Submit { text, .. }) = argv(&args).unwrap().action else {
                panic!("not a plain submission: {args:?}")
            };
            assert_eq!(text, expected);
        }
        for args in [
            vec!["submit", "--wait"],
            vec!["submit", "--timeout", "5", "现在几点"],
            vec!["submit", "--wait", "--timeout", "0", "现在几点"],
            vec!["submit", "--wait", "--timeout", "soon", "现在几点"],
            vec!["submit", "--wait", "--timeout"],
        ] {
            assert!(argv(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn direct_reads_name_one_capability_with_object_arguments() {
        let Action::Request(Command::Read { capability, args }) =
            argv(&["read", "notes.list"]).unwrap().action
        else {
            panic!("not a read")
        };
        assert_eq!(
            (capability.as_str(), args),
            ("notes.list", serde_json::json!({}))
        );
        let Action::Request(Command::Read { args, .. }) =
            argv(&["read", "notes.search", r#"{"q":"牛奶"}"#])
                .unwrap()
                .action
        else {
            panic!("not a read")
        };
        assert_eq!(args, serde_json::json!({"q":"牛奶"}));
        for args in [
            vec!["read"],
            vec!["read", ""],
            vec!["read", "--json"],
            vec!["read", "notes.list", "[]"],
            vec!["read", "notes.list", "not json"],
            vec!["read", "notes.list", "{}", "extra"],
        ] {
            assert!(argv(&args).is_err(), "{args:?}");
        }
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
            ..
        }) = options.action
        else {
            panic!("not list")
        };
        assert_eq!(
            (before, limit, state),
            (Some(TaskId(9)), 5, Some(TaskState::Running))
        );
        assert!(matches!(
            argv(&["tasks", "--state", "unhandled"]).unwrap().action,
            Action::Request(Command::List {
                state: Some(TaskState::Unhandled),
                ..
            })
        ));
        for args in [
            vec!["tasks", "--limit", "0"],
            vec!["tasks", "--limit", "51"],
            vec!["tasks", "--state", "typo"],
            vec!["tasks", "--state", "done"],
            vec!["--socket", "a", "--state-dir", "b", "tasks"],
            vec!["task", "task-0"],
        ] {
            assert!(argv(&args).is_err());
        }
    }

    #[test]
    fn a_thread_is_named_explicitly_and_a_reply_names_its_task_and_step() {
        let Action::Request(Command::Submit { text, thread }) =
            argv(&["submit", "--thread", "task-4", "再加", "鸡蛋"])
                .unwrap()
                .action
        else {
            panic!("not submit")
        };
        assert_eq!((text.as_str(), thread), ("再加 鸡蛋", Some(TaskId(4))));
        // Without the option an intent starts its own thread; the words of
        // an intent are never read as options.
        assert!(matches!(
            argv(&["submit", "说", "--thread", "task-4"])
                .unwrap()
                .action,
            Action::Request(Command::Submit { thread: None, .. })
        ));
        assert!(matches!(
            argv(&["submit", "--thread", "task-4", "--wait", "继续"])
                .unwrap()
                .action,
            Action::SubmitAndWait {
                thread: Some(TaskId(4)),
                ..
            }
        ));
        assert!(argv(&["submit", "--thread", "nope", "继续"]).is_err());
        assert!(matches!(
            argv(&["tasks", "--thread", "task-4"]).unwrap().action,
            Action::Request(Command::List {
                thread: Some(TaskId(4)),
                ..
            })
        ));
        let Action::Request(Command::Reply { id, step, text }) =
            argv(&["reply", "task-7", "2", "下周三", "上午"])
                .unwrap()
                .action
        else {
            panic!("not reply")
        };
        assert_eq!((id, step, text.as_str()), (TaskId(7), 2, "下周三 上午"));
        for args in [
            vec!["reply", "task-7", "2"],
            vec!["reply", "task-7", "0", "好"],
            vec!["reply", "task-7", "2", " "],
        ] {
            assert!(argv(&args).is_err());
        }
        assert_eq!(
            parse_workbench("/reply task-7 2 下周三  上午").unwrap(),
            WorkbenchCommand::Reply {
                id: TaskId(7),
                step: 2,
                text: "下周三  上午".into()
            }
        );
        assert!(parse_workbench("/reply task-7 2").is_err());
        assert_eq!(parse_workbench("/new").unwrap(), WorkbenchCommand::New);
    }

    #[test]
    fn a_preauthorization_is_spelled_out_rule_by_rule_and_its_pin_comes_from_stdin() {
        let options = argv(&[
            "preauthorize",
            "mail.send",
            "--one-of",
            "to=me@example.com,team@example.com",
            "--equals",
            "priority=3",
            "--under",
            "attachment=/home/me/out",
            "--prefix",
            "subject=[bot]",
            "--any",
            "body",
            "--expires",
            "2h",
            "--pin-stdin",
        ])
        .unwrap();
        assert!(options.pin_stdin);
        let Action::Request(Command::Preauthorize {
            capability,
            args,
            other_args,
            expires_in_seconds,
            pin,
        }) = options.action
        else {
            panic!("not a pre-authorization")
        };
        assert_eq!(capability.as_str(), "mail.send");
        assert_eq!(
            args.iter()
                .map(|rule| (rule.field.as_str(), rule.matcher.clone()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "to",
                    Matcher::OneOf(vec![
                        serde_json::json!("me@example.com"),
                        serde_json::json!("team@example.com")
                    ])
                ),
                // A value that reads as JSON is JSON; anything else is text.
                ("priority", Matcher::Equals(serde_json::json!(3))),
                ("attachment", Matcher::Under("/home/me/out".into())),
                ("subject", Matcher::Prefix("[bot]".into())),
                ("body", Matcher::Any),
            ]
        );
        // Unmentioned arguments are refused unless the user says otherwise,
        // and the PIN is never taken from the command line.
        assert!(!other_args && pin.is_none());
        assert_eq!(expires_in_seconds, Some(7_200));
        assert!(matches!(
            argv(&["preauthorize", "x.y", "--any-other-args", "--expires", "90"])
                .unwrap()
                .action,
            Action::Request(Command::Preauthorize {
                other_args: true,
                expires_in_seconds: Some(90),
                ..
            })
        ));
        for args in [
            vec!["preauthorize"],
            vec!["preauthorize", "--equals", "a=1"],
            vec!["preauthorize", "x.y", "--equals", "a"],
            vec!["preauthorize", "x.y", "--equals"],
            vec!["preauthorize", "x.y", "--expires", "0m"],
            vec!["preauthorize", "x.y", "--expires", "2w"],
            vec!["preauthorize", "x.y", "--pin", "1234"],
            vec!["revoke", "task-1"],
            vec!["revoke"],
        ] {
            assert!(argv(&args).is_err(), "{args:?}");
        }
        assert!(matches!(
            argv(&["revoke", "pre-3"]).unwrap().action,
            Action::Request(Command::RevokePreauthorization { id }) if id == "pre-3"
        ));
        assert!(matches!(
            argv(&["preauthorizations"]).unwrap().action,
            Action::Request(Command::Preauthorizations {})
        ));
    }
}
