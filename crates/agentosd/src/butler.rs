//! The butler agent: the system's default entry point for human intents.
//!
//! For each intent it asks the planner for a plan, shows the plan, then runs
//! each step through the policy engine. A step that needs confirmation pauses
//! the plan; the human's next intent is read as the answer.

use std::sync::Arc;

use agentos_audit::{AuditEvent, AuditLog, IntentRecord};
use agentos_core::{AgentId, CapabilityName, Expression, ExpressionKind, Intent, Urgency};
use agentos_policy::{CallContext, Decision, PolicyEngine};
use serde_json::Value;

use crate::expression::ExpressionHub;
use crate::planner::{Plan, Planner};
use crate::registry::CapabilityRegistry;

struct Pending {
    intent: Intent,
    plan: Plan,
    step: usize,
}

pub struct Butler {
    id: AgentId,
    registry: CapabilityRegistry,
    policy: PolicyEngine,
    planner: Box<dyn Planner>,
    audit: Arc<AuditLog>,
    expr: ExpressionHub,
    pending: Option<Pending>,
}

fn is_yes(text: &str) -> bool {
    matches!(
        text.trim().to_lowercase().as_str(),
        "y" | "yes" | "是" | "确认" | "好" | "可以"
    )
}

impl Butler {
    pub fn new(
        id: AgentId,
        registry: CapabilityRegistry,
        policy: PolicyEngine,
        planner: Box<dyn Planner>,
        audit: Arc<AuditLog>,
        expr: ExpressionHub,
    ) -> Self {
        Self {
            id,
            registry,
            policy,
            planner,
            audit,
            expr,
            pending: None,
        }
    }

    pub fn id(&self) -> &AgentId {
        &self.id
    }

    /// Whether a step is waiting for the human's confirmation.
    pub fn awaiting_confirmation(&self) -> bool {
        self.pending.is_some()
    }

    fn log(&self, ev: AuditEvent) {
        if let Err(e) = self.audit.append(ev) {
            eprintln!("audit write failed: {e}");
        }
    }

    async fn say(&self, intent: &Intent, kind: ExpressionKind, text: impl Into<String>) {
        let urgency = if kind == ExpressionKind::AskConfirm {
            Urgency::High
        } else {
            intent.urgency
        };
        self.expr
            .express(Expression {
                from: self.id.clone(),
                intent: Some(intent.id),
                kind,
                urgency,
                text: text.into(),
            })
            .await;
    }

    pub async fn handle(&mut self, intent: Intent) {
        self.log(AuditEvent::IntentReceived {
            intent: Box::new(IntentRecord::from(&intent)),
        });

        if let Some(p) = self.pending.take() {
            let approved = is_yes(&intent.text);
            let cap = p.plan.steps[p.step].capability.clone();
            self.log(AuditEvent::HumanConfirmation {
                intent: p.intent.id,
                capability: cap.clone(),
                approved,
            });
            if approved {
                self.run(p.intent, p.plan, p.step, true).await;
            } else {
                self.say(
                    &p.intent,
                    ExpressionKind::Report,
                    format!("已取消 {cap}，计划中止"),
                )
                .await;
            }
            return;
        }

        let specs = self.registry.specs();
        let plan = self.planner.plan(&intent, &specs).await;
        self.log(AuditEvent::PlanCreated {
            intent: intent.id,
            agent: self.id.clone(),
            steps: plan
                .steps
                .iter()
                .map(|s| s.capability.to_string())
                .collect(),
        });

        if plan.steps.is_empty() {
            let reply = plan
                .reply
                .clone()
                .unwrap_or_else(|| "我还不会做这件事".into());
            self.say(&intent, ExpressionKind::Report, reply).await;
            return;
        }

        let outline = plan
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{}. {}", i + 1, s.capability))
            .collect::<Vec<_>>()
            .join("；");
        self.say(&intent, ExpressionKind::Plan, outline).await;
        self.run(intent, plan, 0, false).await;
    }

    /// Execute `plan` from step `start`. `first_approved` marks the first step
    /// as already confirmed by the human.
    async fn run(&mut self, intent: Intent, plan: Plan, start: usize, first_approved: bool) {
        let ctx = CallContext {
            tainted: plan.tainted,
        };
        for i in start..plan.steps.len() {
            let step = &plan.steps[i];
            let Some(cap) = self.registry.get(&step.capability).cloned() else {
                self.say(
                    &intent,
                    ExpressionKind::Error,
                    format!("没有这个能力：{}", step.capability),
                )
                .await;
                return;
            };
            let decision = if first_approved && i == start {
                Decision::Allow
            } else {
                self.policy.check(&self.id, cap.spec(), ctx)
            };
            self.log(AuditEvent::PolicyDecision {
                intent: intent.id,
                agent: self.id.clone(),
                capability: step.capability.clone(),
                decision: format!("{decision:?}"),
            });

            match decision {
                Decision::Allow => {}
                Decision::Deny { reason } => {
                    self.say(
                        &intent,
                        ExpressionKind::Error,
                        format!("被策略拒绝：{reason}"),
                    )
                    .await;
                    return;
                }
                Decision::NeedsConfirmation {
                    reason,
                    verify_identity,
                } => {
                    let id_note = if verify_identity {
                        "（正式版本会在此要求身份核验）"
                    } else {
                        ""
                    };
                    let text = format!(
                        "{}：{}。原因：{reason}{id_note}。回复 y 确认，其他任意内容取消。",
                        step.capability,
                        cap.spec().description
                    );
                    self.say(&intent, ExpressionKind::AskConfirm, text).await;
                    self.pending = Some(Pending {
                        intent,
                        plan,
                        step: i,
                    });
                    return;
                }
            }

            let result = cap.invoke(step.args.clone()).await;
            let (ok, value) = match &result {
                Ok(v) => (true, v.clone()),
                Err(e) => (false, Value::String(e.to_string())),
            };
            self.log(AuditEvent::CapabilityInvoked {
                intent: intent.id,
                agent: self.id.clone(),
                capability: step.capability.clone(),
                args: step.args.clone(),
                ok,
                result: value.clone(),
            });
            match result {
                Ok(v) => {
                    let msg = v
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string());
                    self.say(&intent, ExpressionKind::Report, msg).await;
                }
                Err(e) => {
                    self.say(
                        &intent,
                        ExpressionKind::Error,
                        format!("{} 失败：{e}", step.capability),
                    )
                    .await;
                    return;
                }
            }
        }
    }
}

/// Grant every capability in `names` to `agent` — what installing an agent
/// with that manifest does.
pub fn grant_manifest(policy: &mut PolicyEngine, agent: &AgentId, names: &[CapabilityName]) {
    for n in names {
        policy.grant(agent, n);
    }
}
