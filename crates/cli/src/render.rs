use agentos_core::now_ms;
use agentos_ipc::{Event, EventKind, Reply, TaskSummary};
use agentos_runtime::{StepState, Task, TaskId, TaskState};

/// Treat terminal output as a device boundary: data cannot contain escape
/// sequences, carriage returns, C1 controls, or invisible direction overrides.
pub fn safe_text(text: &str) -> String {
    let mut safe = String::new();
    for ch in text.chars() {
        match ch {
            '\n' | '\t' => safe.push(ch),
            c if c.is_control()
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}') =>
            {
                safe.push_str(&format!("\\u{{{:x}}}", c as u32));
            }
            c => safe.push(c),
        }
    }
    safe
}

pub fn one_line(text: &str, limit: usize) -> String {
    let safe = safe_text(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = safe.chars();
    let mut line: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        line.push('…');
    }
    line
}

pub fn state_label(state: TaskState) -> &'static str {
    match state {
        TaskState::Queued => "排队中",
        TaskState::Planning => "规划中",
        TaskState::Running => "执行中",
        TaskState::AwaitingConfirmation => "等待批准",
        TaskState::Succeeded => "已完成",
        TaskState::Failed => "失败",
        TaskState::Cancelled => "已取消",
        TaskState::Interrupted => "已中断",
    }
}

fn step_label(state: StepState) -> &'static str {
    match state {
        StepState::Pending => "等待",
        StepState::Running => "执行中",
        StepState::Succeeded => "成功",
        StepState::Failed => "失败",
    }
}

fn bounded(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        result.push_str("\n…（内容较长，使用 --json task 查看完整数据）");
    }
    result
}

fn indent(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn confirmation_visible(task: &Task) -> bool {
    task.confirmation
        .as_ref()
        .and_then(|c| task.steps.get(c.step))
        .is_some_and(|step| {
            safe_text(&step.args.to_string()).chars().count() <= 6_000
                && safe_text(step.capability.as_str()).chars().count() <= 200
        })
}

pub fn task_detail(task: &Task, include_token: bool) -> String {
    let mut lines = vec![
        format!("{} · {}", task.id, state_label(task.state)),
        format!("  意图：{}", one_line(&task.intent.text, 300)),
    ];
    if let Some(c) = &task.confirmation {
        if let Some(step) = task.steps.get(c.step) {
            lines.push(format!(
                "  待确认步骤 {}：{}",
                c.step + 1,
                one_line(step.capability.as_str(), 200)
            ));
            if include_token || confirmation_visible(task) {
                lines.push(indent(&safe_text(&step.args.to_string()), "  完整参数："));
            } else {
                lines.push(
                    "  待批准参数过长，工作台不提供批准；使用 agentos --json task 查看完整数据。"
                        .into(),
                );
            }
        }
    }
    if task.steps.is_empty() {
        lines.push("  尚无工具步骤".into());
    }
    for (i, step) in task.steps.iter().enumerate() {
        lines.push(format!(
            "  {}. {} · {}",
            i + 1,
            one_line(step.capability.as_str(), 120),
            step_label(step.state)
        ));
        lines.push(indent(
            &bounded(&safe_text(&step.args.to_string()), 2_000),
            "     参数：",
        ));
        if let Some(result) = &step.result {
            lines.push(indent(
                &bounded(&safe_text(&result.to_string()), 4_000),
                "     结果：",
            ));
        }
    }
    if let Some(message) = &task.message {
        lines.push(indent(&bounded(&safe_text(message), 4_000), "  "));
    }
    if let Some(confirmation) = &task.confirmation {
        let remaining = confirmation.expires_at_ms.saturating_sub(now_ms()) / 1_000;
        if remaining == 0 {
            lines.push("  批准已过期；检查实际效果后可重新提交意图。".into());
        } else {
            lines.push(format!("  此步骤等待明确批准 · 剩余 {remaining} 秒"));
            if include_token {
                lines.push(format!(
                    "  agentos approve {} {} --confirmation {}",
                    task.id,
                    confirmation.step + 1,
                    one_line(&confirmation.token, 256)
                ));
            } else if confirmation_visible(task) {
                lines.push(format!(
                    "  批准 /approve {} {}    取消 /cancel {}",
                    task.id,
                    confirmation.step + 1,
                    task.id
                ));
            }
        }
    }
    if task.state == TaskState::Cancelled {
        lines.push("  后续步骤已停止；已开始的调用可能仍产生结果。".into());
    }
    lines.join("\n")
}

pub fn task_rows(tasks: &[TaskSummary], selected: Option<TaskId>) -> String {
    if tasks.is_empty() {
        return "  暂无任务。输入意图开始，例如：记一下 买牛奶".into();
    }
    tasks
        .iter()
        .map(|task| {
            format!(
                "{} {:<9} {:<6} {:>2}/{:<2}  {}",
                if selected == Some(task.id) {
                    "▶"
                } else {
                    " "
                },
                task.id.to_string(),
                state_label(task.state),
                task.completed_steps,
                task.total_steps,
                one_line(&task.text, 66)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn dashboard(
    tasks: &[TaskSummary],
    selected: Option<&Task>,
    revision: u64,
    connected: bool,
    page_number: usize,
    has_older: bool,
) -> String {
    let active = tasks
        .iter()
        .filter(|t| {
            matches!(
                t.state,
                TaskState::Queued | TaskState::Planning | TaskState::Running
            )
        })
        .count();
    let waiting = tasks
        .iter()
        .filter(|t| t.state == TaskState::AwaitingConfirmation)
        .count();
    let done = tasks
        .iter()
        .filter(|t| t.state == TaskState::Succeeded)
        .count();
    let attention = tasks
        .iter()
        .filter(|t| matches!(t.state, TaskState::Failed | TaskState::Interrupted))
        .count();
    let mut out = format!("\n╭─ AgentOS v{} · 任务工作台 ─────────────────────────────\n│ {}  · 快照 #{revision} · 第 {page_number} 页\n│ 本页 {} 项  进行中 {active}  待批准 {waiting}  已完成 {done}  需查看 {attention}\n╰────────────────────────────────────────────────────────\n  任务       状态     步骤  意图\n{}\n",env!("CARGO_PKG_VERSION"),if connected { "● 已连接后台" } else { "○ 连接中断，正在重连视图" },tasks.len(),task_rows(tasks,selected.map(|t|t.id)));
    out.push_str(&format!(
        "\n  /prev 上一页  {}  /latest 最近任务\n",
        if has_older {
            "/next 更早任务"
        } else {
            "已到最早一页"
        }
    ));
    out.push_str("\n── 选中任务 ─────────────────────────────────────────────\n");
    match selected {
        Some(task) => out.push_str(&bounded(&task_detail(task, false), 16_000)),
        None => out.push_str("  使用 /task task-N 选择任务。"),
    }
    out.push_str("\n\n── 输入意图或命令 ─────────────────────────────────────────\n  /task 编号 · /cancel 编号 · /help · /quit（后台任务继续）\n");
    out
}

pub fn reply_text(reply: &Reply) -> String {
    match reply {
        Reply::Pong {
            server_version,
            revision,
        } => format!(
            "AgentOS {} · 快照 #{revision}",
            one_line(server_version, 100)
        ),
        Reply::Accepted { task } => format!(
            "[{}] 已接收 · {}。客户端退出后任务继续。\n使用 agentos task {} 查看进度。",
            task.id,
            state_label(task.state),
            task.id
        ),
        Reply::Tasks {
            tasks,
            next_before,
            revision,
        } => {
            let mut text = format!("任务历史 · 快照 #{revision}\n{}", task_rows(tasks, None));
            if let Some(before) = next_before {
                text.push_str(&format!("\n更早任务：agentos tasks --before {before}"));
            }
            text
        }
        Reply::Task { task } => task_detail(task, true),
        Reply::Capabilities { capabilities } => capabilities
            .iter()
            .map(|s| {
                format!(
                    "{} · {:?} · {}",
                    one_line(s.name.as_str(), 100),
                    s.risk,
                    one_line(&s.description, 300)
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Reply::ApprovalQueued { id, step } => {
            format!("{id} 第 {step} 步的批准请求已排队；执行前仍会校验权限与有效期。")
        }
        Reply::Watching { revision } => {
            format!("已订阅任务变更 · 快照 #{revision}；断线后需重新同步任务视图。")
        }
    }
}

pub fn event_text(event: &Event) -> String {
    match &event.kind {
        EventKind::TaskChanged { id } => format!("#{} {id} 状态已更新", event.revision),
        EventKind::ResyncRequired { reason } => format!(
            "#{} 需要重新同步：{}",
            event.revision,
            one_line(reason, 200)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::{DeviceId, Intent, IntentId, Urgency};
    use agentos_runtime::TaskStore;

    #[test]
    fn untrusted_content_cannot_control_the_terminal() {
        let text = "note\x1b[2J\x1b]52;c;clipboard\x07\rspoof\u{009b}31m\u{202e}txt";
        let safe = safe_text(text);
        assert!(!safe.chars().any(|c| c.is_control()));
        assert!(!safe.contains('\u{202e}'));
        assert!(safe.contains("\\u{1b}"));
        assert_eq!(one_line("a\nb\tc", 20), "a b c");
    }

    #[test]
    fn dashboard_shows_real_status_and_results_without_terminal_sequences() {
        let store = TaskStore::memory();
        let task = store
            .create(Intent {
                id: IntentId(0),
                source: DeviceId::new("test"),
                speaker: None,
                text: "记一下 \x1b[2J".into(),
                urgency: Urgency::Normal,
                at_ms: now_ms(),
            })
            .unwrap();
        let task = store
            .update(task.id, |t| {
                t.state = TaskState::Succeeded;
                t.message = Some("已记下：牛奶".into());
            })
            .unwrap();
        let view = dashboard(&[TaskSummary::from(&task)], Some(&task), 3, true, 1, false);
        assert!(
            view.contains("已完成") && view.contains("已记下：牛奶") && view.contains("快照 #3")
        );
        assert!(view.contains("/quit（后台任务继续）"));
        assert!(!view.contains('\x1b'));
    }
}
