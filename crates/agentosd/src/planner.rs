//! Planners turn an intent into a sequence of capability calls.
//!
//! [`RulePlanner`] is a keyword-based stand-in so the runtime works offline
//! and deterministically. An LLM-backed planner (cloud or local) implements the
//! same trait and receives the capability specs as its tool list.
//!
//! Planning is a loop: the agent plans a round of tool calls, runs them, and
//! hands the results back in [`Conversation::history`] so the planner can
//! decide the next round (or answer). Single-shot planners only implement
//! [`Planner::plan`]; the default [`Planner::plan_turn`] ends after one round.

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
\n- 名字以 agent__ 开头的工具是其他专门的 Agent。属于它们领域的多步任务，交给它们去做：task 里写清目标和你需要拿回的结果。\
它们只能用自己被授权的工具，做完会把结果交还给你。\
\n- 每轮工具调用的结果会交还给你，你可以根据结果继续调用工具。事情做完后用一两句话总结，不要重复每一步已经汇报过的内容。\
\n- 闲聊、知识问答等不需要工具的，用一两句话直接回答，不要编造工具。\
\n- 用用户使用的语言回复，简洁。";

/// System prompt for an agent the butler delegates to.
pub fn sub_agent_prompt(name: &str, description: &str, extra: Option<&str>) -> String {
    let mut s = format!(
        "你是 AgentOS 中的「{name}」：{description}。\
管家 Agent 把用户的一部分任务交给了你，你通过调用工具完成它。\
\n- 只用给你的工具；做不到就直接说明原因，不要编造。\
\n- 工具调用会先经过系统的权限检查，高风险操作会由系统向用户请求确认，你不需要自己再问。\
\n- 每轮工具调用的结果会交还给你，你可以继续调用工具。\
\n- 完成后用一两句话说明结果，这段话会交还给管家，由管家向用户汇报。"
    );
    if let Some(extra) = extra.filter(|e| !e.trim().is_empty()) {
        s.push_str("\n\n");
        s.push_str(extra.trim());
    }
    s
}

/// Tool names must match `^[a-zA-Z0-9_-]{1,64}$`; capability names use dots.
pub fn tool_name(cap: &CapabilityName) -> String {
    cap.as_str().replace('.', "__")
}

pub fn capability_name(tool: &str) -> CapabilityName {
    CapabilityName::new(tool.replace("__", "."))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// The model's id for this tool call, echoed back with its result.
    /// Empty for steps a planner made up itself; the agent assigns one.
    pub id: String,
    pub capability: CapabilityName,
    pub args: Value,
}

impl Step {
    pub fn new(capability: CapabilityName, args: Value) -> Self {
        Self {
            id: String::new(),
            capability,
            args,
        }
    }
}

/// The outcome of one tool call, as handed back to the planner.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub id: String,
    pub ok: bool,
    /// What the tool returned (JSON text) or why it failed.
    pub content: String,
}

/// One finished round of an agent's work.
#[derive(Debug, Clone, PartialEq)]
pub struct Round {
    /// What the model said alongside its calls.
    pub text: Option<String>,
    /// The calls that actually ran, in order (including follow-ups).
    pub calls: Vec<Step>,
    pub results: Vec<ToolResult>,
}

/// Everything a planner needs to decide an agent's next round.
pub struct Conversation<'a> {
    pub system: &'a str,
    /// The request: the human's intent for the butler, the task for a sub-agent.
    pub intent: &'a Intent,
    /// Rounds already run, oldest first. Empty on the first call.
    pub history: &'a [Round],
    pub available: &'a [CapabilitySpec],
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
    /// Plan the first (and, for single-shot planners, only) round.
    async fn plan(&self, intent: &Intent, available: &[CapabilitySpec]) -> Plan;

    /// Plan the next round given what has run so far. An empty plan ends the
    /// agent's turn; its `reply` is the agent's answer.
    async fn plan_turn(&self, conv: &Conversation<'_>) -> Plan {
        if conv.history.is_empty() {
            self.plan(conv.intent, conv.available).await
        } else {
            Plan::default()
        }
    }
}

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

#[async_trait]
impl Planner for RulePlanner {
    async fn plan(&self, intent: &Intent, available: &[CapabilitySpec]) -> Plan {
        let t = intent.text.trim();
        let lower = t.to_lowercase();
        let has = |keys: &[&str]| keys.iter().any(|k| lower.contains(k));

        // "@notes 记一下 买牛奶": hand the rest to that agent, if it exists.
        if let Some(rest) = t.strip_prefix('@') {
            let (who, task) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let cap = CapabilityName::new(format!("agent.{who}"));
            if !task.trim().is_empty() && available.iter().any(|s| s.name == cap) {
                return step(cap.as_str(), json!({ "task": task.trim() }));
            }
        }

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
