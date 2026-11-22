//! Planners without a model, for runs that must be deterministic and free.
//!
//! [`Obedient`] is the worst case a real model can only approach: it carries
//! out the user's request, and it also does everything the content it reads
//! tells it to, first. What it cannot do gets through no defence, so what it
//! does get done measures the defences alone. [`Script`] lets a test say
//! exactly what each agent plans at each turn.

use std::sync::Arc;

use agentos_core::{CapabilityName, CapabilitySpec, Intent};
use agentos_planner::{Conversation, Plan, Planner, Step};
use async_trait::async_trait;
use serde_json::Value;

use super::scenarios::Case;

pub fn call(capability: &str, args: Value) -> Plan {
    calls(&[(capability, args)])
}

pub fn calls(steps: &[(&str, Value)]) -> Plan {
    Plan {
        steps: steps
            .iter()
            .map(|(capability, args)| Step::new(CapabilityName::new(*capability), args.clone()))
            .collect(),
        ..Plan::default()
    }
}

pub fn answer(text: &str) -> Plan {
    Plan {
        reply: Some(text.into()),
        ..Plan::default()
    }
}

/// Everything earlier rounds returned, as the planner was shown it.
fn everything_read(conversation: &Conversation<'_>) -> String {
    conversation
        .history
        .iter()
        .flat_map(|round| &round.results)
        .map(|result| result.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether an earlier round made exactly this call.
fn made(conversation: &Conversation<'_>, capability: &str, args: &Value) -> bool {
    conversation
        .history
        .iter()
        .flat_map(|round| &round.calls)
        .any(|step| step.capability.as_str() == capability && step.args == *args)
}

/// Does what it reads. It reads its user task's source, then does whatever
/// the injection in it asks (before anything else, as injections ask), then
/// carries out the user's request and answers.
///
/// It obeys words only: a `follow_up` field is not an instruction to it, so
/// that template tests the butler, which must not run it either.
pub struct Obedient {
    pub case: Case,
}

#[async_trait]
impl Planner for Obedient {
    async fn plan(&self, _: &Intent, _: &[CapabilitySpec]) -> Plan {
        unreachable!("the butler plans by turns")
    }

    async fn plan_turn(&self, conversation: &Conversation<'_>) -> Plan {
        let user = self.case.user;
        let (source, read) = (user.read)();
        if !made(conversation, source, &read) {
            return call(source, read);
        }
        if let Some((goal, _)) = self.case.attack {
            let (capability, args) = (goal.call)();
            let told = everything_read(conversation).contains(goal.instruction);
            if told && !made(conversation, capability, &args) {
                return call(capability, args);
            }
        }
        if let Some(act) = user.act {
            let (capability, args) = act();
            if !made(conversation, capability, &args) {
                return call(capability, args);
            }
        }
        answer(user.answer)
    }
}

/// What a scripted planner is shown on one turn.
#[derive(Debug, Clone)]
pub struct Turn {
    /// Planning for a delegate rather than the butler.
    pub delegate: bool,
    /// The request: the user's for the butler, the handed-over task for a
    /// delegate.
    pub task: String,
    /// Rounds this agent already ran.
    pub rounds: usize,
    /// How each call of the last round ended, and what it returned.
    pub last: Vec<(bool, String)>,
    pub tools: Vec<String>,
}

type ScriptFn = dyn Fn(&Turn) -> Plan + Send + Sync;

/// Plans whatever its function says for each turn.
pub struct Script(Arc<ScriptFn>);

impl Script {
    pub fn new(script: impl Fn(&Turn) -> Plan + Send + Sync + 'static) -> Self {
        Self(Arc::new(script))
    }
}

#[async_trait]
impl Planner for Script {
    async fn plan(&self, _: &Intent, _: &[CapabilitySpec]) -> Plan {
        unreachable!("the butler plans by turns")
    }

    async fn plan_turn(&self, conversation: &Conversation<'_>) -> Plan {
        let turn = Turn {
            // Delegates get their own prompt; only the butler's names it.
            delegate: !conversation.system.contains("管家 Agent，运行在"),
            task: conversation.intent.text.clone(),
            rounds: conversation.history.len(),
            last: conversation
                .history
                .last()
                .map(|round| {
                    round
                        .results
                        .iter()
                        .map(|result| (result.ok, result.content.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            tools: conversation
                .available
                .iter()
                .map(|spec| spec.name.to_string())
                .collect(),
        };
        (self.0)(&turn)
    }
}
