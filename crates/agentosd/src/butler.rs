//! The butler agent: the system's default entry point for human intents, and
//! the dispatcher for every other agent.
//!
//! For each intent the butler runs a planning loop: the planner proposes a
//! round of tool calls, each call goes through the policy engine, and the
//! results go back to the planner, which either plans another round or
//! answers. A tool named `agent.<id>` hands a task to another agent (see
//! [`crate::agents`]); that agent runs the same loop with its own, narrower
//! set of capabilities, and its answer becomes the result of the call.
//!
//! Work in progress is an explicit stack of frames (butler at the bottom, at
//! most one delegate above it). A step that needs the human's confirmation
//! pauses the whole stack; the human's next intent is read as the answer,
//! whichever agent asked.

use std::sync::Arc;

use agentos_audit::{AuditEvent, AuditLog, IntentRecord};
use agentos_core::{
    AgentId, CapabilityName, CapabilitySpec, Expression, ExpressionKind, Intent, Urgency,
};
use agentos_policy::{CallContext, Decision, PolicyEngine};
use serde_json::{json, Value};

use crate::agents::{AgentDirectory, AgentManifest};
use crate::expression::ExpressionHub;
use crate::planner::{Conversation, Plan, Planner, Round, Step, ToolResult, SYSTEM_PROMPT};
use crate::plugin_store::PluginManager;
use crate::registry::CapabilityRegistry;
use crate::shell_api::{risk_label, EventBus};
use crate::voice::MIC_DEVICE;

/// Planning rounds the butler gets per intent.
pub const BUTLER_MAX_ROUNDS: u32 = 8;

/// One agent's work on one request.
struct Frame {
    agent: AgentId,
    /// Display name: "管家" or the delegate's name.
    name: String,
    system: String,
    /// The human's intent for the butler; the task (same intent id) for a delegate.
    intent: Intent,
    specs: Vec<CapabilitySpec>,
    history: Vec<Round>,
    /// The round being executed.
    plan: Option<Plan>,
    next: usize,
    results: Vec<ToolResult>,
    rounds: u32,
    max_rounds: u32,
    tainted: bool,
}

impl Frame {
    fn is_butler(&self) -> bool {
        self.agent.as_str() == "butler"
    }

    /// Key that identifies the current round's plan for shells.
    fn plan_key(&self) -> String {
        format!("{}:{}:{}", self.intent.id.0, self.agent, self.rounds)
    }

    fn current(&self) -> Option<&Step> {
        self.plan.as_ref().and_then(|p| p.steps.get(self.next))
    }

    /// Record `msg` as the current step's failure, skip the rest of the round
    /// (later steps may depend on it) and let the planner decide what next.
    fn fail_rest(&mut self, msg: &str) {
        let Some(plan) = &self.plan else { return };
        for (i, s) in plan.steps.iter().enumerate().skip(self.next) {
            self.results.push(ToolResult {
                id: s.id.clone(),
                ok: false,
                content: if i == self.next {
                    msg.to_string()
                } else {
                    "未执行：前面的步骤没有成功".into()
                },
            });
        }
        self.next = plan.steps.len();
    }

    /// What a delegate hands back when its planner gave no closing words.
    fn summary(&self) -> String {
        let done: Vec<String> = self
            .history
            .iter()
            .flat_map(|r| r.calls.iter().zip(&r.results))
            .filter(|(_, res)| res.ok)
            .map(|(c, _)| c.capability.to_string())
            .collect();
        if done.is_empty() {
            "没有完成任何步骤".into()
        } else {
            format!("已完成：{}", done.join("、"))
        }
    }
}

struct Pending {
    intent: Intent,
    stack: Vec<Frame>,
    /// The step needs identity verification (L3): a voice answer cannot approve it.
    verify_identity: bool,
}

enum Flow {
    Continue,
    /// The top frame is done; its answer, if any.
    Finished(Option<String>),
    /// Waiting for the human.
    Paused {
        verify_identity: bool,
    },
    /// The top frame handed its current call to a new agent.
    Delegate(Box<Frame>),
}

pub struct Butler {
    id: AgentId,
    registry: CapabilityRegistry,
    policy: PolicyEngine,
    planner: Box<dyn Planner>,
    audit: Arc<AuditLog>,
    expr: ExpressionHub,
    pending: Option<Pending>,
    plugins: Option<Arc<PluginManager>>,
    plugin_generation: u64,
    plugin_names: Vec<CapabilityName>,
    events: Option<EventBus>,
    agents: AgentDirectory,
    calls: u64,
}

fn is_yes(text: &str) -> bool {
    // Speech recognizers add punctuation: "确认。", "好的！".
    let t = text
        .trim()
        .trim_matches(|c: char| c.is_ascii_punctuation() || "。！？，、～ ".contains(c))
        .to_lowercase();
    matches!(t.as_str(), "y" | "yes" | "是" | "确认" | "好" | "可以")
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
            plugins: None,
            plugin_generation: 0,
            plugin_names: Vec::new(),
            events: None,
            agents: AgentDirectory::new(),
            calls: 0,
        }
    }

    /// Let the butler use self-generated plugins. Installing a plugin is the
    /// human's confirmed decision, so the butler is granted every active one.
    pub fn with_plugins(mut self, pm: Arc<PluginManager>) -> Self {
        self.plugins = Some(pm);
        self.sync_plugins();
        self
    }

    /// Publish structured progress (plan, step status, confirmations) to shells.
    pub fn with_events(mut self, bus: EventBus) -> Self {
        self.events = Some(bus);
        self
    }

    /// Agents the butler may delegate to. Installing an agent grants the
    /// butler its delegation capability, `agent.<id>`.
    pub fn with_agents(mut self, agents: AgentDirectory) -> Self {
        for m in agents.all() {
            self.policy.grant(&self.id, &m.delegate_name());
        }
        self.agents = agents;
        self
    }

    pub fn agents(&self) -> &AgentDirectory {
        &self.agents
    }

    fn emit(&self, v: Value) {
        if let Some(bus) = &self.events {
            let _ = bus.send(v);
        }
    }

    fn spec_of(&self, name: &CapabilityName) -> Option<CapabilitySpec> {
        self.registry
            .get(name)
            .map(|c| c.spec().clone())
            .or_else(|| self.agents.by_capability(name).map(|m| m.delegate_spec()))
    }

    fn emit_plan(&self, f: &Frame) {
        if self.events.is_none() {
            return;
        }
        let Some(plan) = &f.plan else { return };
        let steps: Vec<Value> = plan
            .steps
            .iter()
            .map(|s| {
                let spec = self.spec_of(&s.capability);
                json!({
                    "capability": s.capability.to_string(),
                    "description": spec.as_ref().map(|s| s.description.clone()).unwrap_or_default(),
                    "risk": spec.map(|s| risk_label(s.risk)),
                })
            })
            .collect();
        self.emit(json!({
            "type": "plan",
            "intent": f.intent.id,
            "plan": f.plan_key(),
            "agent": f.agent,
            "agent_name": f.name,
            "text": f.intent.text,
            "steps": steps,
        }));
    }

    fn emit_step(&self, f: &Frame, status: &str) {
        self.emit(json!({
            "type": "step",
            "intent": f.intent.id,
            "plan": f.plan_key(),
            "agent": f.agent,
            "index": f.next,
            "status": status,
        }));
    }

    /// Bring the registry and grants in line with the active plugins.
    fn sync_plugins(&mut self) {
        let Some(pm) = self.plugins.clone() else {
            return;
        };
        let generation = pm.generation();
        if generation == self.plugin_generation {
            return;
        }
        self.plugin_generation = generation;
        for name in self.plugin_names.drain(..) {
            self.registry.unregister(&name);
            self.policy.revoke(&self.id, &name);
        }
        for cap in pm.active() {
            let name = cap.spec().name.clone();
            if self.registry.get(&name).is_some() {
                // Never let a plugin shadow a built-in capability.
                continue;
            }
            self.registry.register(cap);
            self.policy.grant(&self.id, &name);
            self.plugin_names.push(name);
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
        self.say_as(&self.id, intent, kind, text).await;
    }

    async fn say_as(
        &self,
        from: &AgentId,
        intent: &Intent,
        kind: ExpressionKind,
        text: impl Into<String>,
    ) {
        let urgency = if kind == ExpressionKind::AskConfirm {
            Urgency::High
        } else {
            intent.urgency
        };
        self.expr
            .express(Expression {
                from: from.clone(),
                intent: Some(intent.id),
                kind,
                urgency,
                text: text.into(),
            })
            .await;
    }

    /// Speak for the frame's agent. Delegates are named, since the human only
    /// talks to the butler.
    async fn say_for(&self, f: &Frame, kind: ExpressionKind, text: impl Into<String>) {
        let text = text.into();
        let text = if f.is_butler() {
            text
        } else {
            format!("〔{}〕{text}", f.name)
        };
        self.say_as(&f.agent, &f.intent, kind, text).await;
    }

    pub async fn handle(&mut self, intent: Intent) {
        let id = intent.id;
        self.handle_inner(intent).await;
        if self.pending.is_none() {
            self.emit(json!({"type": "done", "intent": id}));
        }
    }

    async fn handle_inner(&mut self, intent: Intent) {
        self.log(AuditEvent::IntentReceived {
            intent: Box::new(IntentRecord::from(&intent)),
        });

        if let Some(p) = self.pending.take() {
            let approved = is_yes(&intent.text);
            let top = p.stack.last().expect("a paused stack has frames");
            let cap = top
                .current()
                .map(|s| s.capability.clone())
                .expect("a paused frame has a current step");
            if approved && p.verify_identity && intent.source.as_str() == MIC_DEVICE {
                // Anyone in the room (or a TV) could say "确认"; L3 needs the screen.
                let intent_ref = p.intent.clone();
                self.pending = Some(p);
                self.say(
                    &intent_ref,
                    ExpressionKind::AskConfirm,
                    format!(
                        "{cap}：不可逆的操作不能用语音确认。请在屏幕上点“确认执行”，或者说“取消”。"
                    ),
                )
                .await;
                return;
            }
            self.log(AuditEvent::HumanConfirmation {
                intent: p.intent.id,
                capability: cap.clone(),
                approved,
            });
            if approved {
                self.drive(p.intent, p.stack, true).await;
            } else {
                self.emit_step(top, "cancelled");
                for f in p.stack.iter().rev().skip(1) {
                    self.emit_step(f, "cancelled");
                }
                self.say(
                    &p.intent,
                    ExpressionKind::Report,
                    format!("已取消 {cap}，计划中止"),
                )
                .await;
            }
            return;
        }

        self.sync_plugins();
        let mut specs = self.registry.specs();
        specs.extend(self.agents.all().map(AgentManifest::delegate_spec));
        let frame = Frame {
            agent: self.id.clone(),
            name: "管家".into(),
            system: SYSTEM_PROMPT.into(),
            intent: intent.clone(),
            specs,
            history: Vec::new(),
            plan: None,
            next: 0,
            results: Vec::new(),
            rounds: 0,
            max_rounds: BUTLER_MAX_ROUNDS,
            tainted: false,
        };
        self.drive(intent, vec![frame], false).await;
    }

    /// Run the stack until the butler answers or a step waits for the human.
    /// `approved` marks the top frame's current step as confirmed.
    async fn drive(&mut self, human: Intent, mut stack: Vec<Frame>, mut approved: bool) {
        loop {
            match self.advance(&mut stack, &mut approved).await {
                Flow::Continue => {}
                Flow::Delegate(child) => stack.push(*child),
                Flow::Paused { verify_identity } => {
                    self.pending = Some(Pending {
                        intent: human,
                        stack,
                        verify_identity,
                    });
                    return;
                }
                Flow::Finished(answer) => {
                    let done = stack.pop().expect("a finished frame");
                    let Some(parent) = stack.last_mut() else {
                        if let Some(a) = answer {
                            self.say(&human, ExpressionKind::Report, a).await;
                        }
                        return;
                    };
                    // A delegate's answer is the result of the parent's call.
                    let ok = done.history.iter().any(|r| r.results.iter().any(|x| x.ok))
                        || answer.is_some();
                    let content = answer.unwrap_or_else(|| done.summary());
                    self.log(AuditEvent::AgentFinished {
                        intent: human.id,
                        agent: done.agent.clone(),
                        ok,
                        summary: content.clone(),
                    });
                    parent.tainted |= done.tainted;
                    let id = parent.current().map(|s| s.id.clone()).unwrap_or_default();
                    parent.results.push(ToolResult { id, ok, content });
                    let parent = stack.last().expect("parent");
                    self.emit_step(parent, if ok { "done" } else { "failed" });
                    stack.last_mut().expect("parent").next += 1;
                }
            }
        }
    }

    /// One step of the top frame: plan a round, close a round, or run a call.
    async fn advance(&mut self, stack: &mut [Frame], approved: &mut bool) -> Flow {
        self.sync_plugins();
        let depth = stack.len();
        let f = stack.last_mut().expect("non-empty stack");

        // Plan the next round.
        if f.plan.is_none() {
            if f.rounds >= f.max_rounds {
                let msg = format!("已经连续执行了 {} 轮，先停在这里。", f.max_rounds);
                self.say_for(f, ExpressionKind::Report, msg.clone()).await;
                return Flow::Finished(Some(msg));
            }
            let mut plan = self
                .planner
                .plan_turn(&Conversation {
                    system: &f.system,
                    intent: &f.intent,
                    history: &f.history,
                    available: &f.specs,
                })
                .await;
            f.rounds += 1;
            self.log(AuditEvent::PlanCreated {
                intent: f.intent.id,
                agent: f.agent.clone(),
                steps: plan
                    .steps
                    .iter()
                    .map(|s| s.capability.to_string())
                    .collect(),
            });
            if plan.steps.is_empty() {
                if f.history.is_empty() && plan.reply.is_none() && f.is_butler() {
                    plan.reply = Some("我还不会做这件事".into());
                }
                return Flow::Finished(plan.reply);
            }
            for s in plan.steps.iter_mut().filter(|s| s.id.is_empty()) {
                self.calls += 1;
                s.id = format!("call_{}", self.calls);
            }
            f.tainted |= plan.tainted;
            if let Some(reply) = &plan.reply {
                if f.is_butler() {
                    self.say_for(f, ExpressionKind::Report, reply.clone()).await;
                }
            }
            let outline = plan
                .steps
                .iter()
                .enumerate()
                .map(|(i, s)| format!("{}. {}", i + 1, s.capability))
                .collect::<Vec<_>>()
                .join("；");
            f.plan = Some(plan);
            f.next = 0;
            f.results.clear();
            self.emit_plan(f);
            self.say_for(f, ExpressionKind::Plan, outline).await;
            return Flow::Continue;
        }

        // Close a finished round and hand the results back to the planner.
        let Some(step) = f.current().cloned() else {
            let plan = f.plan.take().expect("checked above");
            f.history.push(Round {
                text: plan.reply,
                calls: plan.steps,
                results: std::mem::take(&mut f.results),
            });
            return Flow::Continue;
        };
        let ctx = CallContext { tainted: f.tainted };

        // Delegation: start the agent on its task.
        if let Some(m) = self.agents.by_capability(&step.capability).cloned() {
            let decision = if depth > 1 {
                Decision::Deny {
                    reason: "子 Agent 不能再委派任务".into(),
                }
            } else {
                self.policy.check(&f.agent, &m.delegate_spec(), ctx)
            };
            self.log(AuditEvent::PolicyDecision {
                intent: f.intent.id,
                agent: f.agent.clone(),
                capability: step.capability.clone(),
                decision: format!("{decision:?}"),
            });
            if let Decision::Deny { reason } = decision {
                return self.deny(f, &reason).await;
            }
            let task = step
                .args
                .get("task")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default()
                .to_string();
            if task.is_empty() {
                self.emit_step(f, "failed");
                f.fail_rest("缺少 task：要交给它做什么？");
                return Flow::Continue;
            }
            let child = self.start_delegate(f, &m, task);
            self.emit_step(f, "running");
            self.say_for(
                f,
                ExpressionKind::Plan,
                format!("交给{}：{}", m.name, child.intent.text),
            )
            .await;
            return Flow::Delegate(Box::new(child));
        }

        let Some(cap) = self.registry.get(&step.capability).cloned() else {
            self.emit_step(f, "failed");
            let msg = format!("没有这个能力：{}", step.capability);
            self.say_for(f, ExpressionKind::Error, msg.clone()).await;
            f.fail_rest(&msg);
            return Flow::Continue;
        };
        let decision = if std::mem::take(approved) {
            Decision::Allow
        } else {
            self.policy.check(&f.agent, cap.spec(), ctx)
        };
        self.log(AuditEvent::PolicyDecision {
            intent: f.intent.id,
            agent: f.agent.clone(),
            capability: step.capability.clone(),
            decision: format!("{decision:?}"),
        });

        match decision {
            Decision::Allow => {}
            Decision::Deny { reason } => return self.deny(f, &reason).await,
            Decision::NeedsConfirmation {
                reason,
                verify_identity,
            } => {
                let id_note = if verify_identity {
                    "（正式版本会在此要求身份核验）"
                } else {
                    ""
                };
                let who = if f.is_butler() {
                    String::new()
                } else {
                    format!("{}请求执行 ", f.name)
                };
                let text = format!(
                    "{who}{}：{}。原因：{reason}{id_note}。回复 y 确认，其他任意内容取消。",
                    step.capability,
                    cap.spec().description
                );
                self.emit_step(f, "confirm");
                self.emit(json!({
                    "type": "confirm",
                    "intent": f.intent.id,
                    "plan": f.plan_key(),
                    "agent": f.agent,
                    "agent_name": f.name,
                    "index": f.next,
                    "capability": step.capability.to_string(),
                    "description": cap.spec().description,
                    "risk": risk_label(cap.spec().risk),
                    "reason": reason,
                    "verify_identity": verify_identity,
                    "args": step.args,
                }));
                // Asked by the butler for everyone: the human only talks to it.
                self.say(&f.intent, ExpressionKind::AskConfirm, text).await;
                return Flow::Paused { verify_identity };
            }
        }

        self.emit_step(f, "running");
        let result = cap.invoke(step.args.clone()).await;
        let (ok, value) = match &result {
            Ok(v) => (true, v.clone()),
            Err(e) => (false, Value::String(e.to_string())),
        };
        self.log(AuditEvent::CapabilityInvoked {
            intent: f.intent.id,
            agent: f.agent.clone(),
            capability: step.capability.clone(),
            args: step.args.clone(),
            ok,
            result: value.clone(),
        });
        match result {
            Ok(v) => {
                // A trusted capability may ask for a next step (for example,
                // a proposal asks to be installed). It still goes through policy.
                // Plugins cannot do this: their output is stripped of `follow_up`.
                self.emit_step(f, "done");
                let msg = v
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string());
                f.results.push(ToolResult {
                    id: step.id.clone(),
                    ok: true,
                    content: v.to_string(),
                });
                if let Some(mut next) = follow_up(&v) {
                    self.calls += 1;
                    next.id = format!("call_{}", self.calls);
                    let at = f.next + 1;
                    f.plan
                        .as_mut()
                        .expect("running plan")
                        .steps
                        .insert(at, next);
                    self.emit_plan(f);
                }
                self.say_for(f, ExpressionKind::Report, msg).await;
                f.next += 1;
            }
            Err(e) => {
                self.emit_step(f, "failed");
                let msg = format!("{} 失败：{e}", step.capability);
                self.say_for(f, ExpressionKind::Error, msg.clone()).await;
                f.fail_rest(&msg);
            }
        }
        Flow::Continue
    }

    async fn deny(&self, f: &mut Frame, reason: &str) -> Flow {
        self.emit_step(f, "denied");
        let msg = format!("被策略拒绝：{reason}");
        self.say_for(f, ExpressionKind::Error, msg.clone()).await;
        f.fail_rest(&msg);
        Flow::Continue
    }

    /// Build the delegate's frame and grant it exactly what it may use.
    fn start_delegate(&mut self, parent: &Frame, m: &AgentManifest, task: String) -> Frame {
        let holder = parent.agent.clone();
        let specs = m.effective(&self.registry.specs(), |c| {
            self.policy.is_granted(&holder, c)
        });
        let names: Vec<CapabilityName> = specs.iter().map(|s| s.name.clone()).collect();
        let agent = m.agent_id();
        self.policy.set_grants(&agent, names.clone());
        self.log(AuditEvent::Delegated {
            intent: parent.intent.id,
            from: parent.agent.clone(),
            to: agent.clone(),
            task: task.clone(),
            capabilities: names,
        });
        Frame {
            agent,
            name: m.name.clone(),
            system: crate::planner::sub_agent_prompt(
                &m.name,
                &m.description,
                m.instructions.as_deref(),
            ),
            intent: Intent {
                speaker: None,
                text: task,
                ..parent.intent.clone()
            },
            specs,
            history: Vec::new(),
            plan: None,
            next: 0,
            results: Vec::new(),
            rounds: 0,
            max_rounds: m.max_rounds,
            tainted: parent.tainted,
        }
    }
}

fn follow_up(v: &Value) -> Option<Step> {
    let f = v.get("follow_up")?;
    Some(Step::new(
        CapabilityName::new(f.get("capability")?.as_str()?),
        f.get("args")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
    ))
}

/// Grant every capability in `names` to `agent` — what installing an agent
/// with that manifest does.
pub fn grant_manifest(policy: &mut PolicyEngine, agent: &AgentId, names: &[CapabilityName]) {
    for n in names {
        policy.grant(agent, n);
    }
}
