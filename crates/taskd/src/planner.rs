//! Planners turn an intent into a sequence of capability calls.
//!
//! [`RulePlanner`] is an explicit-command stand-in so the runtime works offline
//! and deterministically. An LLM-backed planner (cloud or local) implements the
//! same trait and receives the capability specs as its tool list.

use agentos_core::{CapabilityName, CapabilitySpec, Intent};
use async_trait::async_trait;
use serde_json::{json, Value};

/// System prompt shared by the LLM-backed planners.
pub const SYSTEM_PROMPT: &str = "你是 AgentOS 的管家 Agent，运行在用户的桌面工作站上。\
用户通过键盘或语音向你表达意图，你通过调用工具完成任务。\
\n\n- 能用工具完成的，就调用对应的工具；需要多个步骤时，按执行顺序依次调用。\
\n- 工具调用会先经过系统的权限检查，高风险操作会由系统向用户请求确认，你不需要自己再问。\
\n- 没有合适的工具时，用一两句话直接回答或说明做不到，不要编造工具。\
\n- 用用户使用的语言回复，简洁。";

/// Tool names must match `^[a-zA-Z0-9_-]{1,64}$`; capability names use dots.
pub fn tool_name(cap: &CapabilityName) -> String {
    cap.as_str().replace('.', "__")
}

pub fn capability_name(tool: &str) -> CapabilityName {
    CapabilityName::new(tool.replace("__", "."))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub capability: CapabilityName,
    pub args: Value,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// Text to show when there is nothing to execute.
    pub reply: Option<String>,
    /// Planning failed; no step from this plan may execute.
    pub error: Option<String>,
    /// The planner had no way to act on the intent. `reply` explains what it
    /// can do instead. This is neither a failure nor an answer: the task ends
    /// as unhandled rather than succeeded.
    pub unhandled: bool,
    /// The plan was influenced by untrusted external content.
    pub tainted: bool,
}

impl Plan {
    pub fn failed(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            reply: Some(message.clone()),
            error: Some(message),
            ..Self::default()
        }
    }

    pub fn unhandled(message: impl Into<String>) -> Self {
        Self {
            reply: Some(message.into()),
            unhandled: true,
            ..Self::default()
        }
    }
}

#[async_trait]
pub trait Planner: Send + Sync {
    async fn plan(&self, intent: &Intent, available: &[CapabilitySpec]) -> Plan;
}

#[derive(Debug, Default)]
pub struct RulePlanner;

fn step(name: &str, args: Value) -> Plan {
    Plan {
        steps: vec![Step {
            capability: CapabilityName::new(name),
            args,
        }],
        ..Plan::default()
    }
}

fn strip_any<'a>(text: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes.iter().find_map(|p| text.strip_prefix(p))
}

#[async_trait]
impl Planner for RulePlanner {
    async fn plan(&self, intent: &Intent, available: &[CapabilitySpec]) -> Plan {
        let t = intent.text.trim();
        let lower = t.to_lowercase();

        if let Some(rest) = strip_any(t, &["记一下", "记住", "note "]) {
            let text = rest.trim_start_matches([':', '：', ' ']).trim();
            return step("notes.append", json!({ "text": text }));
        }
        let normalized = lower.trim_end_matches(['。', '.', '！', '!']).trim();
        let command = strip_any(normalized, &["请帮我", "帮我", "请"])
            .unwrap_or(normalized)
            .trim_start();
        // Mutations require a complete command. Negations, questions, quoted
        // commands and explanations must never become writes by substring match.
        match command {
            "恢复笔记" | "撤销清空" | "撤销清空笔记" | "restore notes" => {
                return step("notes.restore", json!({}));
            }
            "清空笔记" | "清空全部笔记" | "清空所有笔记" | "删除笔记" | "删除全部笔记"
            | "删除所有笔记" | "clear notes" | "clear all notes" => {
                return step("notes.clear", json!({}));
            }
            _ => {}
        }
        let read_command = strip_any(normalized, &["请帮我", "请问", "帮我", "请"])
            .unwrap_or(normalized)
            .trim()
            .trim_end_matches(['?', '？'])
            .trim_end();
        match read_command {
            "笔记" | "看看笔记" | "查看笔记" | "列出笔记" | "列出所有笔记" | "notes"
            | "list notes" | "show notes" => return step("notes.list", json!({})),
            "时间" | "几点" | "几点了" | "现在几点" | "现在几点了" | "现在时间" | "当前时间"
            | "time" | "what time is it" => return step("clock.now", json!({})),
            _ => {}
        }
        let list = available
            .iter()
            .map(|s| format!("- {}：{}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");
        Plan::unhandled(format!(
            "我还不会处理这句话。当前可用的能力：\n{list}\n试试：\"记一下 买牛奶\"、\"看看笔记\"、\"清空笔记\"、\"恢复笔记\"、\"现在几点\""
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::{now_ms, DeviceId, IntentId, Urgency};

    fn intent(text: &str) -> Intent {
        Intent {
            id: IntentId(1),
            source: DeviceId::new("t"),
            speaker: None,
            text: text.into(),
            urgency: Urgency::Normal,
            at_ms: now_ms(),
        }
    }

    async fn first(text: &str) -> Option<String> {
        RulePlanner
            .plan(&intent(text), &[])
            .await
            .steps
            .first()
            .map(|s| s.capability.to_string())
    }

    #[tokio::test]
    async fn keywords_map_to_capabilities() {
        assert_eq!(
            first("记一下：买牛奶").await.as_deref(),
            Some("notes.append")
        );
        assert_eq!(first("清空笔记").await.as_deref(), Some("notes.clear"));
        assert_eq!(first("看看笔记").await.as_deref(), Some("notes.list"));
        assert_eq!(first("恢复笔记").await.as_deref(), Some("notes.restore"));
        assert_eq!(first("撤销清空").await.as_deref(), Some("notes.restore"));
        assert_eq!(
            first("restore notes").await.as_deref(),
            Some("notes.restore")
        );
        assert_eq!(first("现在几点").await.as_deref(), Some("clock.now"));
        assert_eq!(first("讲个笑话").await, None);
    }

    #[tokio::test]
    async fn note_text_is_extracted() {
        let p = RulePlanner.plan(&intent("记一下：买牛奶"), &[]).await;
        assert_eq!(p.steps[0].args, json!({"text": "买牛奶"}));
        let p = RulePlanner.plan(&intent("note clear notes?"), &[]).await;
        assert_eq!(p.steps[0].capability.as_str(), "notes.append");
        assert_eq!(p.steps[0].args, json!({"text": "clear notes?"}));
    }

    #[tokio::test]
    async fn complete_mutating_commands_allow_polite_prefixes_and_terminal_punctuation() {
        for text in [
            "清空笔记",
            "请清空笔记。",
            "帮我清空笔记！",
            "请帮我 清空笔记!",
            "CLEAR NOTES",
            "  clear notes.  ",
        ] {
            assert_eq!(first(text).await.as_deref(), Some("notes.clear"), "{text}");
        }
        for text in [
            "恢复笔记",
            "请恢复笔记。",
            "帮我撤销清空！",
            "请帮我 撤销清空笔记!",
            "RESTORE NOTES",
            "  restore notes.  ",
        ] {
            assert_eq!(
                first(text).await.as_deref(),
                Some("notes.restore"),
                "{text}"
            );
        }
    }

    #[tokio::test]
    async fn negations_questions_and_quoted_commands_do_not_mutate_notes() {
        for text in [
            "不要清空笔记",
            "请不要清空笔记",
            "别恢复笔记",
            "怎么清空笔记",
            "清空笔记吗",
            "清空笔记?",
            "清空笔记？！",
            "请问清空笔记",
            "恢复笔记？",
            "帮我恢复笔记？",
            "Do not clear notes",
            "can you restore notes?",
            "clear notes?",
            "\"clear notes\"",
            "“清空笔记”",
            "这句话是清空笔记",
            "清空笔记后怎么恢复笔记",
        ] {
            let p = RulePlanner.plan(&intent(text), &[]).await;
            assert!(
                p.steps.is_empty(),
                "unexpected action for {text}: {:?}",
                p.steps
            );
            // Not understood is reported as such, never as an answer or an error.
            assert!(p.unhandled && p.error.is_none(), "{text}");
            assert!(p.reply.unwrap().contains("恢复笔记"));
        }
    }

    #[tokio::test]
    async fn only_the_fallback_is_marked_unhandled() {
        for text in ["记一下 买牛奶", "看看笔记", "现在几点", "清空笔记"] {
            assert!(
                !RulePlanner.plan(&intent(text), &[]).await.unhandled,
                "{text}"
            );
        }
        let plan = RulePlanner.plan(&intent("讲个笑话"), &[]).await;
        assert!(plan.unhandled && plan.steps.is_empty() && plan.error.is_none());
        assert!(!Plan::failed("网络错误").unhandled);
    }

    #[tokio::test]
    async fn explicit_read_only_questions_remain_available() {
        for text in ["看看笔记", "请查看笔记？", "SHOW NOTES?"] {
            assert_eq!(first(text).await.as_deref(), Some("notes.list"), "{text}");
        }
        for text in ["现在几点", "请问现在几点？", "What Time Is It?"] {
            assert_eq!(first(text).await.as_deref(), Some("clock.now"), "{text}");
        }
    }
}
