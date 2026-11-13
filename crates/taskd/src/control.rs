//! A deterministic control plane. Commands are never sent to a planner.

use agentos_core::{now_ms, Intent};
use agentos_runtime::{Task, TaskId, TaskState};

pub const HELP: &str = "输入自然语言提交任务，例如：记一下 买牛奶、看看笔记、清空笔记、恢复笔记、现在几点。\n/tasks                 查看最近任务\n/task task-1           查看计划、步骤和结果\n/cancel task-1         取消任务（在途步骤可能已产生效果）\n/approve task-1 1      批准指定任务的第 1 步（五分钟有效）\n/reply task-1 1 内容   回答指定任务第 1 步提出的问题\n/capabilities          查看能力和风险等级\n/help                  显示帮助\nCtrl-D 等待队列完成后退出。";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Tasks,
    Task(TaskId),
    Cancel(TaskId),
    Approve { id: TaskId, step: usize },
    Capabilities,
}

/// Check both when admitting a command and when consuming it. In particular,
/// never queue an approval before its concrete confirmation request exists.
pub fn approval_matches(task: &Task, step: usize, responder: &Intent, at_ms: u64) -> bool {
    task.state == TaskState::AwaitingConfirmation
        && may_approve(task, responder)
        && task
            .confirmation
            .as_ref()
            .is_some_and(|c| c.step == step && at_ms < c.expires_at_ms)
}

fn submitted_by(task: &Task, responder: &Intent) -> bool {
    task.intent.source == responder.source && task.intent.speaker == responder.speaker
}

/// Whoever submitted a task approves its steps, with one exception: a task
/// an outside caller submitted is approved by the human at this machine and
/// never by a caller, itself included. Confirmation always returns to a
/// person.
fn may_approve(task: &Task, responder: &Intent) -> bool {
    match crate::butler::caller_of(&task.intent) {
        Some(_) => crate::butler::caller_of(responder).is_none(),
        None => submitted_by(task, responder),
    }
}

/// A reply answers one concrete question: the one this task is waiting on
/// now, asked at this step, and only from whoever submitted the task.
pub fn reply_matches(task: &Task, step: usize, responder: &Intent) -> bool {
    // A caller's question is about its own request: it may answer, and so
    // may the human, who can always speak for their own machine.
    task.state == TaskState::AwaitingInput
        && (submitted_by(task, responder) || may_approve(task, responder))
        && task.question.as_ref().is_some_and(|q| q.step == step)
}

pub fn approval_matches_with_token(
    task: &Task,
    step: usize,
    responder: &Intent,
    at_ms: u64,
    token: &str,
) -> bool {
    !token.is_empty()
        && approval_matches(task, step, responder, at_ms)
        && task.confirmation.as_ref().is_some_and(|c| c.token == token)
}

/// `None` is ordinary human input. Unknown/malformed slash commands fail
/// locally so a model can never reinterpret an approval or cancellation.
pub fn parse(text: &str) -> Result<Option<Command>, String> {
    let text = text.trim();
    if !text.starts_with('/') {
        return Ok(None);
    }
    let parts: Vec<_> = text.split_whitespace().collect();
    let id = |s: &str| {
        s.parse::<TaskId>()
            .map_err(|_| "任务编号应为 task-1 或 1".to_string())
    };
    let command = match parts.as_slice() {
        ["/help"] => Command::Help,
        ["/tasks"] => Command::Tasks,
        ["/capabilities"] => Command::Capabilities,
        ["/task", task] => Command::Task(id(task)?),
        ["/cancel", task] => Command::Cancel(id(task)?),
        ["/approve", task, step] => {
            let step = step
                .parse::<usize>()
                .ok()
                .and_then(|s| s.checked_sub(1))
                .ok_or("步骤编号必须是从 1 开始的正整数")?;
            Command::Approve {
                id: id(task)?,
                step,
            }
        }
        _ => return Err("未知命令或参数错误，输入 /help 查看用法".into()),
    };
    Ok(Some(command))
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

pub fn task_detail(task: &Task) -> String {
    let mut text = format!(
        "{} · {}\n意图：{}",
        task.id,
        state_label(task.state),
        task.intent.text
    );
    for (i, step) in task.steps.iter().enumerate() {
        text.push_str(&format!(
            "\n{}. {} {} · {:?}",
            i + 1,
            step.capability,
            step.args,
            step.state
        ));
        if let Some(result) = &step.result {
            text.push_str(&format!("\n   {result}"));
        }
    }
    if let Some(message) = &task.message {
        text.push_str(&format!("\n{message}"));
    }
    if let Some(confirmation) = &task.confirmation {
        text.push_str(&format!(
            "\n批准：/approve {} {}（剩余 {} 秒，过期后需重新提交意图）",
            task.id,
            confirmation.step + 1,
            confirmation.expires_at_ms.saturating_sub(now_ms()) / 1_000
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::{DeviceId, IntentId, Urgency};
    use agentos_runtime::{Confirmation, TaskStep, TaskStore};
    use serde_json::json;

    #[test]
    fn approval_is_explicit_and_one_based() {
        assert_eq!(
            parse("/approve task-12 2"),
            Ok(Some(Command::Approve {
                id: TaskId(12),
                step: 1
            }))
        );
        for input in [
            "/approve task-12",
            "/approve task-12 0",
            "/approve task-12 -1",
            "/approve task-12 1 extra",
            "/cancel",
            "/tasks extra",
            "/unknown",
        ] {
            assert!(parse(input).is_err(), "{input}");
        }
        assert_eq!(parse("y"), Ok(None));
        assert_eq!(parse("记一下 /cancel task-1"), Ok(None));
    }

    #[test]
    fn approval_cannot_be_admitted_before_a_concrete_prompt_exists() {
        let store = TaskStore::memory();
        let responder = Intent {
            id: IntentId(1),
            source: DeviceId::new("stdin"),
            speaker: None,
            text: "/approve task-1 1".into(),
            urgency: Urgency::Normal,
            at_ms: now_ms(),
        };
        let task = store.create(responder.clone()).unwrap();
        assert!(!approval_matches(&task, 0, &responder, now_ms()));
        let task = store
            .update(task.id, |task| {
                task.state = TaskState::Planning;
            })
            .unwrap();
        assert!(!approval_matches(&task, 0, &responder, now_ms()));
        let task = store
            .update(task.id, |task| {
                task.state = TaskState::AwaitingConfirmation;
                task.steps = vec![TaskStep::pending(
                    agentos_core::CapabilityName::new("mail.send"),
                    json!({"to":"alice"}),
                )];
                task.confirmation = Some(Confirmation {
                    step: 0,
                    expires_at_ms: now_ms() + 1_000,
                    token: "test-token".into(),
                    verify_identity: false,
                });
            })
            .unwrap();
        assert!(approval_matches(&task, 0, &responder, now_ms()));
    }
}
