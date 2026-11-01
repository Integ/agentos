//! The judge: a cheap second opinion on a step the policy engine would let
//! through without asking the human (in the spirit of jev's damage control).
//!
//! The policy engine decides by risk tier alone; it cannot see that
//! "append a note" is harmless while "write a file over ~/.ssh/config" is not.
//! Before an allowed step that changes something (L1 and up) runs, the judge
//! looks at the call in context. It can only tighten a decision: a flag turns
//! "allow" into "ask the human", never the other way round. If the judge is
//! unavailable the step runs, as the policy engine already allowed it, and the
//! failure is audited.

use std::sync::Arc;

use agentos_core::CapabilitySpec;
use async_trait::async_trait;
use serde_json::Value;

use crate::llm::TextModel;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Ask the human first; the reason is shown to them.
    Flag(String),
}

pub struct Review<'a> {
    /// Agent doing the call ("butler" or a delegate's id).
    pub agent: &'a str,
    /// What the agent is working on.
    pub task: &'a str,
    pub spec: &'a CapabilitySpec,
    pub args: &'a Value,
    /// The task's context includes untrusted external content.
    pub tainted: bool,
}

#[async_trait]
pub trait Judge: Send + Sync {
    async fn review(&self, r: &Review<'_>) -> Result<Verdict, String>;
}

const JUDGE_PROMPT: &str = "你是 AgentOS 的安全裁判。一个 Agent 正要执行一个已经通过权限检查、不会再问用户的操作。\
判断这个操作放在当前任务里是否合理、是否可能造成用户不想要的损失（删除或覆盖数据、泄露隐私、花钱、对外发送、超出任务范围）。\
\n只输出一行：\n- 合理就输出 ALLOW\n- 有疑问就输出 FLAG: 后面跟一句给用户看的中文理由";

/// A judge backed by a text model.
pub struct LlmJudge {
    model: Arc<dyn TextModel>,
}

impl LlmJudge {
    pub fn new(model: Arc<dyn TextModel>) -> Self {
        Self { model }
    }
}

pub fn parse_verdict(text: &str) -> Verdict {
    let t = text.trim();
    let upper = t.to_uppercase();
    if upper.starts_with("ALLOW") {
        return Verdict::Allow;
    }
    let reason = t
        .split_once([':', '：'])
        .map(|(_, r)| r.trim())
        .filter(|r| !r.is_empty())
        .unwrap_or("裁判认为这一步需要你确认");
    // Anything that is not a clear ALLOW is a flag: fail towards the human.
    Verdict::Flag(reason.to_string())
}

#[async_trait]
impl Judge for LlmJudge {
    async fn review(&self, r: &Review<'_>) -> Result<Verdict, String> {
        let user = format!(
            "Agent：{}\n任务：{}\n操作：{}（{}，风险 {:?}）\n参数：{}\n上下文含不可信的外部内容：{}",
            r.agent,
            r.task,
            r.spec.name,
            r.spec.description,
            r.spec.risk,
            r.args,
            if r.tainted { "是" } else { "否" }
        );
        let out = self.model.complete(JUDGE_PROMPT, &user).await?;
        Ok(parse_verdict(&out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_parse_and_fail_towards_the_human() {
        assert_eq!(parse_verdict(" ALLOW"), Verdict::Allow);
        assert_eq!(
            parse_verdict("FLAG: 会覆盖已有文件"),
            Verdict::Flag("会覆盖已有文件".into())
        );
        assert!(matches!(parse_verdict("我不确定"), Verdict::Flag(_)));
    }
}
