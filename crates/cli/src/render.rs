use agentos_core::{now_ms, RiskLevel};
use agentos_ipc::{
    Caller, Event, EventKind, Matcher, Preauthorization, Reply, Subject, TaskSummary,
};
use agentos_runtime::{StepState, Task, TaskId, TaskState};

use crate::wait::WaitOutcome;

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
        TaskState::AwaitingInput => "等待回答",
        TaskState::Succeeded => "已完成",
        TaskState::Unhandled => "未能处理",
        TaskState::Failed => "失败",
        TaskState::Cancelled => "已取消",
        TaskState::Interrupted => "已中断",
    }
}

/// `L0`..`L3`, the same labels the documentation and confirmations use.
pub fn risk_label(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::ReadOnly => "L0 只读",
        RiskLevel::Reversible => "L1 可恢复",
        RiskLevel::External => "L2 对外",
        RiskLevel::Irreversible => "L3 不可逆",
    }
}

fn step_label(state: StepState) -> &'static str {
    match state {
        StepState::Pending => "等待",
        StepState::Running => "执行中",
        StepState::Succeeded => "成功",
        StepState::Failed => "失败",
        StepState::Skipped => "未执行",
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
        lines.push(
            if task.state == TaskState::Unhandled {
                "  没有执行任何步骤：这句话没有对应的能力"
            } else if task.state.is_terminal() {
                "  没有执行工具步骤"
            } else {
                "  尚无工具步骤"
            }
            .into(),
        );
    }
    for (i, step) in task.steps.iter().enumerate() {
        // A step taken by a delegate is shown under the agent that took it.
        let by = step
            .agent
            .as_ref()
            .map(|agent| format!("〔{}〕", one_line(agent.as_str(), 40)))
            .unwrap_or_default();
        lines.push(format!(
            "  {}. {by}{} · {}",
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
    let used = &task.usage;
    if used.model_calls > 0 {
        let mut line = format!(
            "  用量：{} 次模型请求 · 输入 {} / 输出 {} token · 执行 {} 秒",
            used.model_calls,
            used.input_tokens,
            used.output_tokens,
            used.active_ms / 1_000
        );
        if used.unmetered_calls > 0 {
            // Unknown is not zero: say that the counts are a lower bound.
            line.push_str(&format!(
                "（其中 {} 次没有报告用量，token 数只是下限）",
                used.unmetered_calls
            ));
        }
        lines.push(line);
    }
    if let Some(question) = &task.question {
        // The question itself is the task's message, shown above.
        lines.push(format!(
            "  任务在等你回答第 {} 步的问题；不回答就不会继续。",
            question.step + 1
        ));
        lines.push(if include_token {
            format!(
                "  agentos reply {} {} <你的回答>",
                task.id,
                question.step + 1
            )
        } else {
            format!(
                "  回答 /reply {} {} <你的回答>    取消 /cancel {}",
                task.id,
                question.step + 1,
                task.id
            )
        });
    }
    if task.state == TaskState::Cancelled {
        lines.push("  后续步骤已停止；已开始的调用可能仍产生结果。".into());
    }
    lines.join("\n")
}

/// One caller and exactly what it was given.
pub fn caller_line(caller: &Caller) -> String {
    format!(
        "{} · {} · 最高 {} · {}",
        one_line(&caller.id, 40),
        one_line(&caller.name, 80),
        risk_label(caller.max_risk),
        if caller.capabilities.is_empty() {
            "没有任何能力".to_string()
        } else {
            caller
                .capabilities
                .iter()
                .map(|pattern| one_line(pattern, 80))
                .collect::<Vec<_>>()
                .join("、")
        }
    )
}

/// One pre-authorization, with every rule spelled out: what is not written
/// here is not permitted by it.
pub fn preauthorization_line(pre: &Preauthorization) -> String {
    let value = |v: &serde_json::Value| one_line(&v.to_string(), 120);
    let mut rules: Vec<String> = pre
        .args
        .iter()
        .map(|rule| {
            let field = one_line(&rule.field, 60);
            match &rule.matcher {
                Matcher::Equals(v) => format!("{field} = {}", value(v)),
                Matcher::OneOf(vs) => format!(
                    "{field} 属于 [{}]",
                    vs.iter().map(value).collect::<Vec<_>>().join(", ")
                ),
                Matcher::Prefix(text) => format!("{field} 以 {} 开头", one_line(text, 120)),
                Matcher::Under(dir) => format!("{field} 在目录 {} 内", one_line(dir, 120)),
                Matcher::Any => format!("{field} 任意"),
            }
        })
        .collect();
    rules.push(if pre.other_args {
        "其他参数任意".into()
    } else {
        "不允许其他参数".into()
    });
    let expiry = match pre.expires_at_ms {
        Some(at) => format!("剩余 {} 分钟", at.saturating_sub(now_ms()).div_ceil(60_000)),
        None => "不过期".into(),
    };
    let subject = match &pre.subject {
        Subject::User => "你自己的任务".to_string(),
        Subject::Caller(id) => format!("调用方 {} 的任务", one_line(id, 40)),
    };
    format!(
        "{} · {subject} · {} · {} · {expiry}",
        one_line(&pre.id, 40),
        one_line(pre.capability.as_str(), 100),
        rules.join("；")
    )
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
    let asking = tasks
        .iter()
        .filter(|t| t.state == TaskState::AwaitingInput)
        .count();
    let done = tasks
        .iter()
        .filter(|t| t.state == TaskState::Succeeded)
        .count();
    let attention = tasks
        .iter()
        .filter(|t| {
            matches!(
                t.state,
                TaskState::Failed | TaskState::Interrupted | TaskState::Unhandled
            )
        })
        .count();
    let mut out = format!("\n╭─ AgentOS v{} · 任务工作台 ─────────────────────────────\n│ {}  · 快照 #{revision} · 第 {page_number} 页\n│ 本页 {} 项  进行中 {active}  待批准 {waiting}  待回答 {asking}  已完成 {done}  需查看 {attention}\n╰────────────────────────────────────────────────────────\n  任务       状态     步骤  意图\n{}\n",env!("CARGO_PKG_VERSION"),if connected { "● 已连接后台" } else { "○ 连接中断，正在重连视图" },tasks.len(),task_rows(tasks,selected.map(|t|t.id)));
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
            "[{}] 已接收 · {}。客户端退出后任务继续。\n使用 agentos task {} 查看进度；接着说用 agentos submit --thread {} <意图>。",
            task.id,
            state_label(task.state),
            task.id,
            task.thread
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
                    "{} · {} · {}",
                    one_line(s.name.as_str(), 100),
                    risk_label(s.risk),
                    one_line(&s.description, 300)
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Reply::Value { value, .. } => {
            // Capabilities describe their own result in `message`; anything
            // else is shown as data. Either way it is untrusted for a terminal.
            let text = match value.get("message").and_then(|m| m.as_str()) {
                Some(message) => message.to_string(),
                None => serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
            };
            bounded(&safe_text(&text), 16_000)
        }
        Reply::ApprovalQueued { id, step } => {
            format!("{id} 第 {step} 步的批准请求已排队；执行前仍会校验权限与有效期。")
        }
        Reply::ReplyQueued { id, step } => {
            format!("{id} 第 {step} 步的回答已收到；任务会接着进行。")
        }
        Reply::Preauthorized { preauthorization } => format!(
            "已创建预授权，符合它的调用不再逐次确认：\n{}\n撤销：agentos revoke {}",
            preauthorization_line(preauthorization),
            one_line(&preauthorization.id, 40)
        ),
        Reply::Preauthorizations { preauthorizations } if preauthorizations.is_empty() => {
            "没有有效的预授权。".into()
        }
        Reply::Preauthorizations { preauthorizations } => preauthorizations
            .iter()
            .map(preauthorization_line)
            .collect::<Vec<_>>()
            .join("\n"),
        Reply::PreauthorizationRevoked { id } => format!(
            "{} 已撤销，立即生效；进行中的任务也不再适用。",
            one_line(id, 40)
        ),
        Reply::CallerRegistered { caller } => format!(
            "已注册外部调用方，它只能使用列出的能力：\n{}\n接入：agentos mcp --caller {}　撤销：agentos caller remove {}",
            caller_line(caller),
            one_line(&caller.id, 40),
            one_line(&caller.id, 40)
        ),
        Reply::Callers { callers } if callers.is_empty() => "没有注册外部调用方。".into(),
        Reply::Callers { callers } => callers
            .iter()
            .map(caller_line)
            .collect::<Vec<_>>()
            .join("\n"),
        Reply::CallerRevoked { id } => format!(
            "调用方 {} 已撤销；它进行中的任务不再能调用任何能力，它的预授权也一并撤销。",
            one_line(id, 40)
        ),
        Reply::Watching { revision } => {
            format!("已订阅任务变更 · 快照 #{revision}；断线后需重新同步任务视图。")
        }
    }
}

/// The closing line of `submit --wait`: what happened, and what to do next.
pub fn wait_text(outcome: &WaitOutcome) -> String {
    let task = outcome.task();
    let detail = task_detail(task, true);
    let note = match outcome {
        WaitOutcome::Settled(_) => return detail,
        WaitOutcome::NeedsApproval(_) => {
            "任务在等待批准，不会自行继续；核对上面的步骤与参数后再批准或取消。".to_string()
        }
        WaitOutcome::NeedsInput(_) => {
            "任务在等待你的回答，不会自行继续；用上面的 reply 命令回答，或取消任务。".to_string()
        }
        WaitOutcome::TimedOut(_) => format!(
            "等待超时；任务仍在后台继续。稍后使用 agentos task {} 查看。",
            task.id
        ),
    };
    format!("{detail}\n{note}")
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

    #[test]
    fn an_unhandled_intent_is_never_shown_as_completed() {
        let store = TaskStore::memory();
        let task = store
            .create(Intent {
                id: IntentId(0),
                source: DeviceId::new("test"),
                speaker: None,
                text: "讲个笑话".into(),
                urgency: Urgency::Normal,
                at_ms: now_ms(),
            })
            .unwrap();
        let task = store
            .update(task.id, |t| {
                t.state = TaskState::Unhandled;
                t.message = Some("我还不会处理这句话。".into());
            })
            .unwrap();
        let detail = task_detail(&task, true);
        assert!(detail.contains("未能处理") && detail.contains("没有对应的能力"));
        assert!(!detail.contains("已完成"));
        let view = dashboard(&[TaskSummary::from(&task)], Some(&task), 2, true, 1, false);
        assert!(view.contains("已完成 0") && view.contains("需查看 1"));
        assert_eq!(WaitOutcome::Settled(task.clone()).exit_code(), 3);
        let done = store
            .create(Intent {
                id: IntentId(0),
                source: DeviceId::new("test"),
                speaker: None,
                text: "现在几点".into(),
                urgency: Urgency::Normal,
                at_ms: now_ms(),
            })
            .unwrap();
        let done = store
            .update(done.id, |t| t.state = TaskState::Succeeded)
            .unwrap();
        assert_eq!(WaitOutcome::Settled(done.clone()).exit_code(), 0);
        assert_eq!(WaitOutcome::TimedOut(done).exit_code(), 5);
        assert!(wait_text(&WaitOutcome::TimedOut(task)).contains("仍在后台继续"));
    }

    #[test]
    fn direct_read_results_and_risk_levels_are_rendered_for_people() {
        let reply = Reply::Value {
            capability: agentos_core::CapabilityName::new("notes.list"),
            value: serde_json::json!({"notes":["牛奶"],"message":"1. 牛奶\x1b[2J"}),
        };
        let text = reply_text(&reply);
        assert!(text.starts_with("1. 牛奶") && !text.contains('\x1b'));
        let raw = Reply::Value {
            capability: agentos_core::CapabilityName::new("x.read"),
            value: serde_json::json!({"count":2}),
        };
        assert!(reply_text(&raw).contains("\"count\": 2"));
        assert_eq!(risk_label(RiskLevel::ReadOnly), "L0 只读");
        assert_eq!(risk_label(RiskLevel::Irreversible), "L3 不可逆");
    }
}
