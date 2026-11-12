//! The offline rule planner: explicit commands, no model.
//!
//! It keeps the runtime usable and deterministic without a model service. It
//! only recognises complete commands. Anything that changes state must be the
//! whole sentence (after a polite prefix): a negation, a question or a quoted
//! command never becomes a write through a keyword match.
//!
//! Notes and the clock are always planned; everything else (tasks, agents, the
//! blackboard, plugins, the smart home) only when that capability is actually
//! offered, so the same planner serves any set of capabilities.

use agentos_core::{CapabilityName, CapabilitySpec, Intent};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Plan, Planner, Step};

#[derive(Debug, Default)]
pub struct RulePlanner;

fn step(name: &str, args: Value) -> Plan {
    Plan {
        steps: vec![Step::new(CapabilityName::new(name), args)],
        ..Plan::default()
    }
}

fn strip_any<'a>(text: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes.iter().find_map(|p| text.strip_prefix(p))
}

fn after_colon(text: &str) -> &str {
    text.trim_start_matches([':', '：', ' ']).trim()
}

#[async_trait]
impl Planner for RulePlanner {
    async fn plan(&self, intent: &Intent, available: &[CapabilitySpec]) -> Plan {
        let t = intent.text.trim();
        let lower = t.to_lowercase();
        let offered = |name: &str| available.iter().any(|s| s.name.as_str() == name);
        let mentions = |keys: &[&str]| keys.iter().any(|k| lower.contains(k));

        // The task table.
        if offered("tasks.list") && matches!(lower.as_str(), "任务" | "看看任务" | "ps" | "tasks")
        {
            return step("tasks.list", json!({}));
        }
        if offered("tasks.cancel") {
            if let Some(rest) = strip_any(t, &["取消任务", "kill "]) {
                if let Ok(n) = rest.trim().trim_start_matches('#').parse::<u64>() {
                    return step("tasks.cancel", json!({ "task": n }));
                }
            }
        }

        // "@notes 记一下 买牛奶": hand the rest to that agent, if it exists.
        if let Some(rest) = t.strip_prefix('@') {
            let (who, task) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let delegate = format!("agent.{who}");
            if !task.trim().is_empty() && offered(&delegate) {
                return step(&delegate, json!({ "task": task.trim() }));
            }
        }

        // The rest of the sentence is the note, whatever it says.
        if let Some(rest) = strip_any(t, &["记一下", "记住", "note "]) {
            return step("notes.append", json!({ "text": after_colon(rest) }));
        }
        if offered("evolve.propose") {
            if let Some(rest) = strip_any(t, &["学会", "学习", "进化", "learn "]) {
                return step("evolve.propose", json!({ "need": after_colon(rest) }));
            }
        }

        let normalized = lower.trim_end_matches(['。', '.', '！', '!']).trim();
        let command = strip_any(normalized, &["请帮我", "帮我", "请"])
            .unwrap_or(normalized)
            .trim_start();
        // Mutations require a complete command. Negations, questions, quoted
        // commands and explanations must never become writes by substring match.
        match command {
            "恢复笔记" | "撤销清空" | "撤销清空笔记" | "restore notes"
                if offered("notes.restore") || available.is_empty() =>
            {
                return step("notes.restore", json!({}));
            }
            "清空笔记" | "清空全部笔记" | "清空所有笔记" | "删除笔记" | "删除全部笔记"
            | "删除所有笔记" | "clear notes" | "clear all notes" => {
                return step("notes.clear", json!({}));
            }
            _ => {}
        }
        // Plugins are named, so these are complete commands with an argument.
        // The name is taken from the original text to keep its case.
        if offered("evolve.rollback") {
            if let Some(rest) = strip_any(t, &["回滚", "恢复", "rollback "]) {
                if !rest.trim().is_empty() {
                    return step("evolve.rollback", json!({ "name": rest.trim() }));
                }
            }
        }
        if offered("evolve.disable") {
            if let Some(rest) = strip_any(t, &["停用", "disable "]) {
                if !rest.trim().is_empty() {
                    return step("evolve.disable", json!({ "name": rest.trim() }));
                }
            }
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
        // Read-only lookups may match on a keyword: nothing can be changed.
        if offered("board.read") && mentions(&["黑板", "board"]) {
            return step("board.read", json!({}));
        }
        if offered("evolve.list") && mentions(&["插件", "plugins", "新能力"]) {
            return step("evolve.list", json!({}));
        }
        if offered("home.list") && mentions(&["智能家居", "家里的设备", "家里设备", "smart home"])
        {
            return step("home.list", json!({}));
        }

        let list = available
            .iter()
            .map(|s| format!("- {}：{}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");
        let mut examples = vec!["\"记一下 买牛奶\"", "\"看看笔记\"", "\"清空笔记\""];
        if offered("notes.restore") || available.is_empty() {
            examples.push("\"恢复笔记\"");
        }
        examples.push("\"现在几点\"");
        let mut reply = format!(
            "我还不会处理这句话。当前可用的能力：\n{list}\n试试：{}",
            examples.join("、")
        );
        if offered("evolve.propose") {
            reply.push_str("，或者\"学会：……\"让我给自己写一个新能力");
        }
        Plan::unhandled(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::{now_ms, DeviceId, IntentId, RiskLevel, Urgency};

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

    fn specs(names: &[&str]) -> Vec<CapabilitySpec> {
        names
            .iter()
            .map(|name| CapabilitySpec {
                name: CapabilityName::new(*name),
                description: String::new(),
                risk: RiskLevel::ReadOnly,
                input_schema: json!({}),
            })
            .collect()
    }

    const EVERYTHING: &[&str] = &[
        "notes.append",
        "notes.list",
        "notes.clear",
        "notes.restore",
        "clock.now",
        "tasks.list",
        "tasks.cancel",
        "board.read",
        "evolve.propose",
        "evolve.rollback",
        "evolve.disable",
        "evolve.list",
        "home.list",
        "agent.notes",
    ];

    async fn first_with(text: &str, offered: &[&str]) -> Option<String> {
        RulePlanner
            .plan(&intent(text), &specs(offered))
            .await
            .steps
            .first()
            .map(|s| s.capability.to_string())
    }

    async fn first(text: &str) -> Option<String> {
        first_with(text, &[]).await
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

    #[tokio::test]
    async fn optional_capabilities_are_planned_only_when_offered() {
        for (text, capability) in [
            ("任务", "tasks.list"),
            ("取消任务 #3", "tasks.cancel"),
            ("看看黑板", "board.read"),
            ("学会：换算温度", "evolve.propose"),
            ("回滚 unit.convert", "evolve.rollback"),
            ("恢复 unit.convert", "evolve.rollback"),
            ("停用 unit.convert", "evolve.disable"),
            ("看看插件", "evolve.list"),
            ("家里的设备有哪些", "home.list"),
            ("@notes 记一下 买牛奶", "agent.notes"),
        ] {
            assert_eq!(
                first_with(text, EVERYTHING).await.as_deref(),
                Some(capability),
                "{text}"
            );
            // Without the capability the sentence is simply not understood.
            let plan = RulePlanner
                .plan(&intent(text), &specs(&["notes.list"]))
                .await;
            assert!(plan.steps.is_empty() && plan.unhandled, "{text}");
        }
        let plan = RulePlanner
            .plan(&intent("回滚 Unit.Convert"), &specs(EVERYTHING))
            .await;
        assert_eq!(plan.steps[0].args, json!({"name": "Unit.Convert"}));
        let plan = RulePlanner
            .plan(&intent("@notes 清空笔记"), &specs(EVERYTHING))
            .await;
        assert_eq!(plan.steps[0].args, json!({"task": "清空笔记"}));
    }

    #[tokio::test]
    async fn restoring_notes_and_re_enabling_a_plugin_do_not_collide() {
        // The exact notebook command wins; a named plugin goes to evolve.
        assert_eq!(
            first_with("恢复笔记", EVERYTHING).await.as_deref(),
            Some("notes.restore")
        );
        assert_eq!(
            first_with("恢复 greet.hello", EVERYTHING).await.as_deref(),
            Some("evolve.rollback")
        );
        // A daemon whose notebook cannot restore does not plan a restore.
        let without_restore: Vec<&str> = EVERYTHING
            .iter()
            .copied()
            .filter(|name| *name != "notes.restore")
            .collect();
        assert_eq!(
            first_with("恢复笔记", &without_restore).await.as_deref(),
            Some("evolve.rollback")
        );
    }

    #[tokio::test]
    async fn the_fallback_only_suggests_what_is_offered() {
        let plan = RulePlanner
            .plan(&intent("讲个笑话"), &specs(&["notes.list", "clock.now"]))
            .await;
        let reply = plan.reply.unwrap();
        assert!(reply.contains("- notes.list") && !reply.contains("恢复笔记"));
        assert!(!reply.contains("学会"));
        let plan = RulePlanner
            .plan(&intent("讲个笑话"), &specs(EVERYTHING))
            .await;
        let reply = plan.reply.unwrap();
        assert!(reply.contains("恢复笔记") && reply.contains("学会"));
    }
}
