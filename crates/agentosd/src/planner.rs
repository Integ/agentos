//! Planners turn an intent into a sequence of capability calls.
//!
//! [`RulePlanner`] is a keyword-based stand-in so the runtime works offline
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
\n- 现有工具做不到用户要的事，而这件事可以写成一个小程序来完成时，调用 evolve__propose 为自己编写新能力，\
need 写清楚要做什么、输入和输出；用户要求改进已安装的插件时，把插件名填进 target。\
不要自己调用 evolve__install，系统会在用户审阅草案后发起安装确认。\
\n- 闲聊、知识问答等不需要工具的，用一两句话直接回答，不要编造工具。\
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
    /// The plan was influenced by untrusted external content.
    pub tainted: bool,
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
        let has = |keys: &[&str]| keys.iter().any(|k| lower.contains(k));

        if let Some(rest) = strip_any(t, &["记一下", "记住", "note "]) {
            let text = rest.trim_start_matches([':', '：', ' ']).trim();
            return step("notes.append", json!({ "text": text }));
        }
        if let Some(rest) = strip_any(t, &["学会", "学习", "进化", "learn "]) {
            let need = rest.trim_start_matches([':', '：', ' ']).trim();
            return step("evolve.propose", json!({ "need": need }));
        }
        if let Some(rest) = strip_any(t, &["回滚", "恢复", "rollback "]) {
            return step("evolve.rollback", json!({ "name": rest.trim() }));
        }
        if let Some(rest) = strip_any(t, &["停用", "disable "]) {
            return step("evolve.disable", json!({ "name": rest.trim() }));
        }
        if has(&["插件", "plugins", "新能力"]) {
            return step("evolve.list", json!({}));
        }
        if has(&["笔记", "notes"]) && has(&["清空", "删除", "clear"]) {
            return step("notes.clear", json!({}));
        }
        if has(&["笔记", "notes"]) {
            return step("notes.list", json!({}));
        }
        if has(&["几点", "时间", "time"]) {
            return step("clock.now", json!({}));
        }
        let list = available
            .iter()
            .map(|s| format!("- {}：{}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");
        Plan {
            reply: Some(format!(
                "我还不会处理这句话。当前可用的能力：\n{list}\n试试：\"记一下 买牛奶\"、\"看看笔记\"、\"清空笔记\"、\"现在几点\"，或者\"学会：……\"让我给自己写一个新能力"
            )),
            ..Plan::default()
        }
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
        assert_eq!(first("现在几点").await.as_deref(), Some("clock.now"));
        assert_eq!(first("讲个笑话").await, None);
        assert_eq!(
            first("学会：换算温度").await.as_deref(),
            Some("evolve.propose")
        );
        assert_eq!(
            first("回滚 unit.convert").await.as_deref(),
            Some("evolve.rollback")
        );
        assert_eq!(first("看看插件").await.as_deref(), Some("evolve.list"));
    }

    #[tokio::test]
    async fn note_text_is_extracted() {
        let p = RulePlanner.plan(&intent("记一下：买牛奶"), &[]).await;
        assert_eq!(p.steps[0].args, json!({"text": "买牛奶"}));
    }
}
