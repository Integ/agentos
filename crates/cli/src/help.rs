//! `agentos help`: the commands in groups, with a page per group and per
//! command, and the same table as JSON for scripts and agents.
//!
//! One table drives every form of help, so the overview, the detailed pages
//! and `agentos --json help` cannot disagree about what exists.

use serde_json::{json, Value};

use crate::style::{paint, Tone};

pub struct Group {
    pub key: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
}

pub struct Entry {
    pub name: &'static str,
    pub group: &'static str,
    /// Without the leading `agentos`.
    pub usage: &'static [&'static str],
    pub summary: &'static str,
    pub details: &'static [&'static str],
}

pub const GROUPS: &[Group] = &[
    Group {
        key: "tasks",
        title: "任务",
        summary: "提交、查看、取消和回答",
    },
    Group {
        key: "approvals",
        title: "批准与授权",
        summary: "批准或只拒绝一步，事先许可一类调用",
    },
    Group {
        key: "callers",
        title: "外部调用方与渠道",
        summary: "别的 Agent 通过 MCP 使用 AgentOS，在微信里说话",
    },
    Group {
        key: "inspect",
        title: "诊断与查看",
        summary: "哪里不对、最近做了什么、有哪些能力",
    },
];

pub const COMMANDS: &[Entry] = &[
    Entry {
        name: "workbench",
        group: "tasks",
        usage: &["workbench"],
        summary: "实时终端工作台（不写命令时默认打开）",
        details: &[
            "普通输入就是意图，接着本次对话说；工作台里输入 /help 查看它的命令。",
            "有任务在等你批准或回答时，任务页上方有一栏“需要你处理”。",
            "/quit 只关闭工作台，后台任务继续运行。",
        ],
    },
    Entry {
        name: "submit",
        group: "tasks",
        usage: &["submit [--thread task-N] [--wait] [--timeout 秒] <意图>"],
        summary: "提交任务；客户端退出后任务继续",
        details: &[
            "--wait 等到任务结束再返回并打印最终任务；--timeout 设置最长等待，默认 300 秒。",
            "--thread 接着某段对话说，用这段对话第一个任务的编号。",
            "选项只认意图前面的；意图本身以 - 开头时先写 --，例如 agentos submit -- --help。",
            "退出码（--wait）：3 任务结束但没有成功；4 在等待批准；5 等待超时，任务仍在后台继续；6 在等待回答。",
        ],
    },
    Entry {
        name: "tasks",
        group: "tasks",
        usage: &["tasks [--before task-N] [--limit N] [--state 状态] [--thread task-N]"],
        summary: "分页查看任务历史",
        details: &[
            "状态：queued、planning、running、awaiting_confirmation、awaiting_input、succeeded、unhandled、failed、cancelled、interrupted。",
            "--limit 每页 1–50 个，默认 20。",
        ],
    },
    Entry {
        name: "task",
        group: "tasks",
        usage: &["task task-N"],
        summary: "查看一个任务的计划、结果和批准令牌",
        details: &[],
    },
    Entry {
        name: "cancel",
        group: "tasks",
        usage: &["cancel task-N"],
        summary: "取消任务的后续步骤；已经开始的那一步可能仍会完成",
        details: &[],
    },
    Entry {
        name: "reply",
        group: "tasks",
        usage: &["reply task-N 步骤 <回答>"],
        summary: "回答任务在这一步提出的问题；任务在原地继续",
        details: &[],
    },
    Entry {
        name: "watch",
        group: "tasks",
        usage: &["watch [--after revision]"],
        summary: "持续接收任务变更和重新同步通知；Ctrl-C 退出，不停止后台任务",
        details: &[],
    },
    Entry {
        name: "approve",
        group: "approvals",
        usage: &["approve task-N 步骤 --confirmation 令牌 [--pin-stdin]"],
        summary: "批准一步",
        details: &[
            "令牌用 agentos task task-N 查看；批准绑定任务、步骤、令牌、系统用户和五分钟期限。",
            "不可逆（L3）的步骤加 --pin-stdin，从标准输入读取 PIN，PIN 不会出现在命令行里。",
            "步骤从 1 开始。",
        ],
    },
    Entry {
        name: "reject",
        group: "approvals",
        usage: &["reject task-N 步骤 --confirmation 令牌"],
        summary: "只拒绝这一步：它不执行，任务换别的办法继续；不需要 PIN",
        details: &[],
    },
    Entry {
        name: "preauthorize",
        group: "approvals",
        usage: &[
            "preauthorize <能力> [规则…] [--any-other-args] [--expires 时长] [--caller id] [--pin-stdin]",
        ],
        summary: "预授权：符合规则的 L1/L2 调用不再逐次确认",
        details: &[
            "规则：--equals 字段=值　--one-of 字段=值1,值2　--prefix 字段=文本　--under 字段=目录　--any 字段。",
            "没写到的参数默认不允许出现，--any-other-args 才放开；时长如 30m、2h、7d，最长 30 天。",
            "不适用于 L3，也不适用于读过外部内容的任务。设置了 PIN 时加 --pin-stdin。",
        ],
    },
    Entry {
        name: "preauthorizations",
        group: "approvals",
        usage: &["preauthorizations"],
        summary: "查看仍然有效的预授权",
        details: &[],
    },
    Entry {
        name: "revoke",
        group: "approvals",
        usage: &["revoke pre-N"],
        summary: "撤销一条预授权，立即生效",
        details: &[],
    },
    Entry {
        name: "callers",
        group: "callers",
        usage: &["callers"],
        summary: "查看已注册的外部调用方",
        details: &[],
    },
    Entry {
        name: "caller",
        group: "callers",
        usage: &[
            "caller add <id> --name 名字 [--allow 能力或前缀]… [--max-risk L0|L1|L2] [--pin-stdin]",
            "caller remove <id>",
        ],
        summary: "注册、整体替换或撤销一个外部调用方",
        details: &[
            "add 默认什么都不给，连只读能力也要用 --allow 写明。",
            "remove 撤销调用方，它的预授权一并撤销，没结束的任务一并取消。",
        ],
    },
    Entry {
        name: "mcp",
        group: "callers",
        usage: &["mcp --caller <id>"],
        summary: "以这个调用方的身份在标准输入输出上提供 MCP 服务",
        details: &["调用方只能用清单里的能力，只看得到自己的任务；有副作用的步骤要你批准。"],
    },
    Entry {
        name: "wechat",
        group: "callers",
        usage: &["wechat login", "wechat"],
        summary: "在微信里和 AgentOS 说话",
        details: &[
            "login 扫码绑定微信官方的 ClawBot（微信：我 → 设置 → 插件）。",
            "之后 agentos wechat 把你发的文字交给 AgentOS，结果发回微信；微信里不能批准。",
        ],
    },
    Entry {
        name: "doctor",
        group: "inspect",
        usage: &["doctor"],
        summary: "诊断：任务服务是否在运行、规划器、沙箱、PIN、上次崩溃，每个问题怎么办",
        details: &[
            "只读，不发送任何东西；--json 输出同样的内容。",
            "退出码：0 没有需要处理的问题；1 连不上任务服务；3 在运行，但有需要处理的问题。",
        ],
    },
    Entry {
        name: "activity",
        group: "inspect",
        usage: &["activity [--limit N]"],
        summary: "最近活动：能力调用、批准与拒绝、录音、崩溃、自动采用的经验",
        details: &["来自审计日志的末尾，最新的在最下面；默认 20 条，最多 100 条。"],
    },
    Entry {
        name: "capabilities",
        group: "inspect",
        usage: &["capabilities"],
        summary: "查看能力和它们的风险等级（L0–L3）",
        details: &[],
    },
    Entry {
        name: "read",
        group: "inspect",
        usage: &["read <能力> [JSON 参数]"],
        summary: "直接读取一个只读（L0）能力，不创建任务",
        details: &[
            "例如 agentos read notes.list，agentos read improve.report '{\"days\":7}'。",
            "有副作用的能力会被拒绝，要用 submit 提交意图。",
        ],
    },
    Entry {
        name: "ping",
        group: "inspect",
        usage: &["ping"],
        summary: "检查连接，显示任务服务的版本",
        details: &[],
    },
];

pub enum Topic {
    Group(&'static Group),
    Command(&'static Entry),
}

/// A command by name, or a group by key or title.
pub fn find(topic: &str) -> Option<Topic> {
    if let Some(entry) = COMMANDS.iter().find(|entry| entry.name == topic) {
        return Some(Topic::Command(entry));
    }
    GROUPS
        .iter()
        .find(|group| group.key == topic || group.title == topic)
        .map(Topic::Group)
}

fn members(group: &Group) -> impl Iterator<Item = &'static Entry> + '_ {
    COMMANDS
        .iter()
        .filter(move |entry| entry.group == group.key)
}

fn group_of(entry: &Entry) -> &'static Group {
    GROUPS
        .iter()
        .find(|group| group.key == entry.group)
        .expect("every command names a group in GROUPS")
}

fn heading(group: &Group) -> String {
    format!("{}（{}）", paint(group.title, Tone::Strong), group.key)
}

fn overview() -> String {
    let mut lines = vec![
        paint("AgentOS · 独立任务工作台", Tone::Strong),
        String::new(),
        "用法：agentos [--socket 路径 | --state-dir 路径] [--json] <命令>".into(),
        "      agentos help [分组或命令]，或者 agentos <命令> --help".into(),
        String::new(),
        paint("常用：", Tone::Strong),
        "  agentos                         打开实时终端工作台".into(),
        "  agentos submit --wait <意图>    提交任务并等它结束".into(),
        "  agentos doctor                  诊断哪里不对、怎么办".into(),
        "  agentos activity                看最近活动".into(),
        String::new(),
    ];
    for group in GROUPS {
        let names: Vec<_> = members(group).map(|entry| entry.name).collect();
        lines.push(format!("{}：{}", heading(group), names.join("  ")));
        lines.push(paint(&format!("  {}", group.summary), Tone::Quiet));
    }
    lines.extend([
        String::new(),
        "--json 放在命令前，输出结构化 JSON；agentos --json help 列出全部命令。".into(),
        "退出码：0 成功；1 连接、协议或后台错误；2 用法错误；个别命令另有，见它的说明。".into(),
        "--version 查看版本。工作台的 /quit 只关闭客户端，不停止后台服务。".into(),
    ]);
    lines.join("\n")
}

fn group_page(group: &Group) -> String {
    let mut lines = vec![format!("{}：{}", heading(group), group.summary)];
    for entry in members(group) {
        for usage in entry.usage {
            lines.push(format!("  agentos {usage}"));
        }
        lines.push(paint(&format!("      {}", entry.summary), Tone::Quiet));
    }
    lines.push(String::new());
    lines.push("agentos help <命令> 查看某条命令的详细说明。".into());
    lines.join("\n")
}

fn command_page(entry: &Entry) -> String {
    let group = group_of(entry);
    let mut lines = vec![
        format!(
            "{} · {}",
            paint(&format!("agentos {}", entry.name), Tone::Strong),
            group.title
        ),
        entry.summary.to_string(),
        String::new(),
        "用法：".into(),
    ];
    lines.extend(entry.usage.iter().map(|usage| format!("  agentos {usage}")));
    if !entry.details.is_empty() {
        lines.push(String::new());
        lines.extend(entry.details.iter().map(|line| format!("  {line}")));
    }
    lines.join("\n")
}

/// Help as text: the overview, or the page for `topic` (already checked
/// with [`find`]).
pub fn text(topic: Option<&str>) -> String {
    match topic.and_then(find) {
        None => overview(),
        Some(Topic::Group(group)) => group_page(group),
        Some(Topic::Command(entry)) => command_page(entry),
    }
}

fn entry_json(entry: &Entry) -> Value {
    json!({
        "name": entry.name,
        "group": entry.group,
        "usage": entry.usage.iter().map(|usage| format!("agentos {usage}")).collect::<Vec<_>>(),
        "summary": entry.summary,
        "details": entry.details,
    })
}

fn group_json(group: &Group) -> Value {
    json!({
        "key": group.key,
        "title": group.title,
        "summary": group.summary,
        "commands": members(group).map(|entry| entry.name).collect::<Vec<_>>(),
    })
}

/// Help as JSON, for scripts and agents: every group and command, or just
/// the ones `topic` names.
pub fn json(topic: Option<&str>) -> Value {
    let (groups, commands): (Vec<Value>, Vec<Value>) = match topic.and_then(find) {
        None => (
            GROUPS.iter().map(group_json).collect(),
            COMMANDS.iter().map(entry_json).collect(),
        ),
        Some(Topic::Group(group)) => (
            vec![group_json(group)],
            members(group).map(entry_json).collect(),
        ),
        Some(Topic::Command(entry)) => (vec![group_json(group_of(entry))], vec![entry_json(entry)]),
    };
    json!({ "groups": groups, "commands": commands })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_command_belongs_to_a_group_and_appears_once() {
        let mut names = HashSet::new();
        for entry in COMMANDS {
            assert!(names.insert(entry.name), "{} listed twice", entry.name);
            assert!(GROUPS.iter().any(|group| group.key == entry.group));
            assert!(!entry.usage.is_empty() && !entry.summary.is_empty());
        }
        for group in GROUPS {
            assert!(members(group).next().is_some(), "{} is empty", group.key);
        }
    }

    #[test]
    fn the_overview_names_every_command_under_its_group() {
        let text = crate::style::with_color(false, || text(None));
        for entry in COMMANDS {
            let line = text
                .lines()
                .find(|line| line.contains(&format!("（{}）", entry.group)))
                .unwrap();
            assert!(
                line.contains(entry.name),
                "{} not under {}",
                entry.name,
                entry.group
            );
        }
        assert!(text.contains("agentos --json help"));
    }

    #[test]
    fn a_group_can_be_named_by_key_or_title_and_a_command_has_its_own_page() {
        let by_key = crate::style::with_color(false, || text(Some("approvals")));
        let by_title = crate::style::with_color(false, || text(Some("批准与授权")));
        assert_eq!(by_key, by_title);
        assert!(by_key.contains("agentos approve") && by_key.contains("agentos revoke"));
        let doctor = crate::style::with_color(false, || text(Some("doctor")));
        assert!(doctor.contains("用法：\n  agentos doctor") && doctor.contains("3 在运行"));
        assert!(find("nonsense").is_none());
    }

    #[test]
    fn json_help_lists_the_same_commands_as_the_text() {
        let all = json(None);
        let names: Vec<_> = all["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, COMMANDS.iter().map(|e| e.name).collect::<Vec<_>>());
        assert_eq!(all["groups"].as_array().unwrap().len(), GROUPS.len());
        let one = json(Some("submit"));
        assert_eq!(
            one["commands"][0]["usage"][0]
                .as_str()
                .unwrap()
                .split_whitespace()
                .next(),
            Some("agentos")
        );
        assert_eq!(one["groups"][0]["key"], "tasks");
    }
}
