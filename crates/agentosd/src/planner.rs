//! Planners turn an intent into a sequence of capability calls.
//!
//! [`RulePlanner`] is a keyword-based stand-in so the runtime works offline
//! and deterministically. An LLM-backed planner (cloud or local) implements the
//! same trait and receives the capability specs as its tool list.

use agentos_core::{CapabilityName, CapabilitySpec, Intent};
use async_trait::async_trait;
use serde_json::{json, Value};

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
                "我还不会处理这句话。当前可用的能力：\n{list}\n试试：\"记一下 买牛奶\"、\"看看笔记\"、\"清空笔记\"、\"现在几点\""
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
    }

    #[tokio::test]
    async fn note_text_is_extracted() {
        let p = RulePlanner.plan(&intent("记一下：买牛奶"), &[]).await;
        assert_eq!(p.steps[0].args, json!({"text": "买牛奶"}));
    }
}
