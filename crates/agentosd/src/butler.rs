//! The butler agent: the system's default entry point for human intents, the
//! dispatcher for every other agent, and the scheduler of its tasks.
//!
//! OS concepts mapped onto agents (see `docs/agents.md`):
//!
//! - every intent is a **task** (a process) with its own id, state and call
//!   stack; a task waiting for the human does not block the next one;
//! - tasks are **checkpointed** to disk after every step and resume after a
//!   restart (see [`crate::tasks`]);
//! - capabilities are **system calls**, gated by the policy engine, and a
//!   **judge** may tighten (never loosen) what policy allowed;
//! - `agent.<id>` hands work to another agent, whose frame runs on the same
//!   task's stack with narrower grants (see [`crate::agents`]);
//! - the **blackboard** is shared memory across tasks and agents, and recent
//!   tasks are fed back to the planner, so context carries across sessions.
//!
//! For each task the butler runs a planning loop: the planner proposes a
//! round of tool calls, each goes through policy, and the results go back to
//! the planner, which plans another round or answers.

use std::collections::BTreeMap;
use std::sync::Arc;

use agentos_audit::{AuditEvent, AuditLog, IntentRecord};
use agentos_core::{
    AgentId, CapabilityName, CapabilitySpec, Expression, ExpressionKind, Intent, RiskLevel, Urgency,
};
use agentos_policy::{CallContext, Decision, PolicyEngine};
use serde_json::{json, Value};

use crate::agents::{AgentDirectory, AgentManifest};
use crate::blackboard::Blackboard;
use crate::expression::ExpressionHub;
use crate::identity::PinVault;
use crate::judge::{Judge, Review, Verdict};
use crate::planner::{Conversation, Planner, Round, Step, ToolResult, SYSTEM_PROMPT};
use crate::plugin_store::PluginManager;
use crate::registry::CapabilityRegistry;
use crate::shell_api::{risk_label, EventBus};
use crate::tasks::{Frame, Task, TaskState, TaskStore, KEEP_FINISHED};
use crate::voice::MIC_DEVICE;

/// Planning rounds the butler gets per task.
pub const BUTLER_MAX_ROUNDS: u32 = 8;

/// Finished tasks the planner is reminded of.
const MEMORY_TASKS: usize = 8;

enum Flow {
    Continue,
    /// The top frame is done; its answer, if any.
    Finished(Option<String>),
    /// Waiting for the human.
    Paused,
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
    plugins: Option<Arc<PluginManager>>,
    plugin_generation: u64,
    plugin_names: Vec<CapabilityName>,
    events: Option<EventBus>,
    agents: AgentDirectory,
    calls: u64,
    /// Unfinished tasks waiting for the human (the running one is on the stack).
    waiting: BTreeMap<u64, Task>,
    /// Finished tasks, oldest first (the agent's task memory).
    recent: Vec<Task>,
    next_task: u64,
    store: Option<TaskStore>,
    /// Unfinished tasks loaded from the store, to be resumed.
    to_resume: Vec<Task>,
    board: Option<Blackboard>,
    judge: Option<Arc<dyn Judge>>,
    /// With a PIN set, L3 approvals need a fresh PIN entry.
    pins: Option<Arc<PinVault>>,
}

fn is_yes(text: &str) -> bool {
    matches!(
        norm(text).as_str(),
        "y" | "yes" | "是" | "确认" | "好" | "可以"
    )
}

fn is_no(text: &str) -> bool {
    matches!(
        norm(text).as_str(),
        "n" | "no" | "否" | "取消" | "算了" | "不" | "不要" | "不用"
    )
}

fn norm(text: &str) -> String {
    // Speech recognizers add punctuation: "确认。", "好的！".
    text.trim()
        .trim_matches(|c: char| c.is_ascii_punctuation() || "。！？，、～ ".contains(c))
        .to_lowercase()
}

/// "y #3" → (Some(3), "y").
fn split_task_ref(text: &str) -> (Option<u64>, String) {
    if let Some(pos) = text.find('#') {
        let digits: String = text[pos + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(n) = digits.parse() {
            let rest = format!("{}{}", &text[..pos], &text[pos + 1 + digits.len()..]);
            return (Some(n), rest);
        }
    }
    (None, text.to_string())
}

fn syscall_specs() -> Vec<CapabilitySpec> {
    vec![
        CapabilitySpec {
            name: CapabilityName::new("tasks.list"),
            description: "列出进行中、等待确认和最近完成的任务".into(),
            risk: RiskLevel::ReadOnly,
            input_schema: json!({"type": "object", "properties": {}}),
        },
        CapabilitySpec {
            name: CapabilityName::new("tasks.cancel"),
            description: "取消一个等待确认的任务".into(),
            risk: RiskLevel::Reversible,
            input_schema: json!({
                "type": "object",
                "properties": {"task": {"type": "integer", "description": "任务编号"}},
                "required": ["task"]
            }),
        },
    ]
}

impl Butler {
    pub fn new(
        id: AgentId,
        registry: CapabilityRegistry,
        mut policy: PolicyEngine,
        planner: Box<dyn Planner>,
        audit: Arc<AuditLog>,
        expr: ExpressionHub,
    ) -> Self {
        for s in syscall_specs() {
            policy.grant(&id, &s.name);
        }
        Self {
            id,
            registry,
            policy,
            planner,
            audit,
            expr,
            plugins: None,
            plugin_generation: 0,
            plugin_names: Vec::new(),
            events: None,
            agents: AgentDirectory::new(),
            calls: 0,
            waiting: BTreeMap::new(),
            recent: Vec::new(),
            next_task: 0,
            store: None,
            to_resume: Vec::new(),
            board: None,
            judge: None,
            pins: None,
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

    /// Shared memory shown to the planner. Register its capabilities with
    /// [`crate::blackboard::register_all`] so agents can read and write it.
    pub fn with_blackboard(mut self, board: Blackboard) -> Self {
        self.board = Some(board);
        self
    }

    /// Require a PIN for irreversible (L3) approvals once one is set.
    pub fn with_pin_vault(mut self, pins: Arc<PinVault>) -> Self {
        self.pins = Some(pins);
        self
    }

    fn pin_required(&self) -> bool {
        self.pins.as_ref().is_some_and(|p| p.is_set())
    }

    /// Second opinion before allowed steps that change something.
    pub fn with_judge(mut self, judge: Arc<dyn Judge>) -> Self {
        self.judge = Some(judge);
        self
    }

    /// Keep tasks on disk. Loads finished tasks as memory and queues
    /// unfinished ones for [`Butler::resume`]. Returns a note per bad file.
    pub fn with_task_store(mut self, store: TaskStore) -> (Self, Vec<String>) {
        store.prune(KEEP_FINISHED);
        let (tasks, notes) = store.load();
        for t in tasks {
            self.next_task = self.next_task.max(t.id);
            if t.state.is_active() {
                self.to_resume.push(t);
            } else {
                self.recent.push(t);
            }
        }
        self.store = Some(store);
        (self, notes)
    }

    pub fn agents(&self) -> &AgentDirectory {
        &self.agents
    }

    /// Unfinished tasks waiting to be resumed after a restart.
    pub fn pending_resume(&self) -> usize {
        self.to_resume.len()
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
            .or_else(|| syscall_specs().into_iter().find(|s| &s.name == name))
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
            "task": f.task,
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
            "task": f.task,
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

    /// Whether any task is waiting for the human's confirmation.
    pub fn awaiting_confirmation(&self) -> bool {
        !self.waiting.is_empty()
    }

    /// Ids of the tasks waiting for the human, oldest first.
    pub fn waiting_tasks(&self) -> Vec<u64> {
        self.waiting.keys().copied().collect()
    }

    fn log(&self, ev: AuditEvent) {
        if let Err(e) = self.audit.append(ev) {
            eprintln!("audit write failed: {e}");
        }
    }

    fn save(&self, task: &mut Task) {
        task.updated_ms = agentos_core::now_ms();
        if let Some(store) = &self.store {
            if let Err(e) = store.save(task) {
                eprintln!("任务 #{} 保存失败：{e}", task.id);
            }
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

    /// What the planner should know beyond the request: recent tasks, tasks
    /// still waiting, and the blackboard.
    fn context(&self) -> String {
        let mut s = SYSTEM_PROMPT.to_string();
        if !self.recent.is_empty() {
            s.push_str("\n\n最近完成的任务（旧的在前，供参考，不需要重做）：");
            let skip = self.recent.len().saturating_sub(MEMORY_TASKS);
            for t in &self.recent[skip..] {
                s.push_str("\n- ");
                s.push_str(&t.line());
            }
        }
        if !self.waiting.is_empty() {
            s.push_str("\n\n正在等待用户确认的任务：");
            for t in self.waiting.values() {
                s.push_str("\n- ");
                s.push_str(&t.line());
            }
        }
        if let Some(b) = self.board.as_ref().and_then(|b| b.summary(1500)) {
            s.push_str("\n\n共享黑板（");
            s.push_str(&b);
            s.push('）');
        }
        s
    }

    pub async fn handle(&mut self, intent: Intent) {
        self.log(AuditEvent::IntentReceived {
            intent: Box::new(IntentRecord::from(&intent)),
        });
        if !self.waiting.is_empty() {
            let (target, rest) = split_task_ref(&intent.text);
            let (yes, no) = (is_yes(&rest), is_no(&rest));
            if yes || no {
                let newest = self.waiting.keys().next_back().copied();
                let id = target.filter(|t| self.waiting.contains_key(t)).or(newest);
                if let Some(id) = id {
                    self.answer(id, yes, &intent).await;
                    return;
                }
            }
        }
        self.start(intent).await;
    }

    /// Resume tasks that were unfinished when agentosd last stopped.
    pub async fn resume(&mut self) {
        for mut task in std::mem::take(&mut self.to_resume) {
            match task.state {
                TaskState::Waiting => {
                    if let Some((text, event)) = task.ask.clone() {
                        self.emit(event);
                        self.say(
                            &task.intent,
                            ExpressionKind::AskConfirm,
                            format!("（重启前未完成的任务 #{}）{text}", task.id),
                        )
                        .await;
                    }
                    self.waiting.insert(task.id, task);
                }
                _ => {
                    let registry = &self.registry;
                    task.recover(|s| registry.get(&s.capability).map(|c| c.spec().risk));
                    self.say(
                        &task.intent,
                        ExpressionKind::Report,
                        format!("继续重启前未完成的任务 #{}：{}", task.id, task.intent.text),
                    )
                    .await;
                    self.run(task, false).await;
                }
            }
        }
    }

    async fn start(&mut self, intent: Intent) {
        self.sync_plugins();
        self.next_task += 1;
        let id = self.next_task;
        let mut specs = self.registry.specs();
        specs.extend(self.agents.all().map(AgentManifest::delegate_spec));
        specs.extend(syscall_specs());
        let frame = Frame {
            task: id,
            agent: self.id.clone(),
            name: "管家".into(),
            system: self.context(),
            intent: intent.clone(),
            specs,
            history: Vec::new(),
            plan: None,
            next: 0,
            results: Vec::new(),
            rounds: 0,
            max_rounds: BUTLER_MAX_ROUNDS,
            tainted: false,
            started: None,
        };
        self.log(AuditEvent::TaskState {
            task: id,
            intent: intent.id,
            state: "running".into(),
        });
        self.run(Task::new(id, intent, frame), false).await;
    }

    async fn answer(&mut self, id: u64, approved: bool, reply: &Intent) {
        let Some(mut task) = self.waiting.remove(&id) else {
            return;
        };
        let cap = task
            .stack
            .last()
            .and_then(Frame::current)
            .map(|s| s.capability.clone())
            .unwrap_or_else(|| CapabilityName::new("?"));
        if approved && task.verify_identity && reply.source.as_str() == MIC_DEVICE {
            // Anyone in the room (or a TV) could say "确认"; L3 needs the screen.
            let intent = task.intent.clone();
            self.waiting.insert(id, task);
            self.say(
                &intent,
                ExpressionKind::AskConfirm,
                format!(
                    "{cap}：不可逆的操作不能用语音确认。请在屏幕上点“确认执行”，或者说“取消”。"
                ),
            )
            .await;
            return;
        }
        let pin_ok = || {
            self.pins
                .as_ref()
                .map_or(true, |p| !p.is_set() || p.take_grant(id))
        };
        if approved && task.verify_identity && !pin_ok() {
            // A bare "y" is not enough for L3: show the sheet again for the PIN.
            let intent = task.intent.clone();
            let event = task.ask.as_ref().map(|(_, e)| e.clone());
            self.waiting.insert(id, task);
            if let Some(mut e) = event {
                e["pin_required"] = json!(true);
                e["pin_error"] = json!("需要输入 PIN");
                self.emit(e);
            }
            self.say(
                &intent,
                ExpressionKind::AskConfirm,
                format!(
                    "{cap}：不可逆的操作需要 PIN。请在屏幕确认框里输入 PIN（控制台先输入“pin 你的PIN”再回复 y），或者说“取消”。"
                ),
            )
            .await;
            return;
        }
        self.log(AuditEvent::HumanConfirmation {
            intent: task.intent.id,
            capability: cap.clone(),
            approved,
        });
        task.ask = None;
        if approved {
            task.state = TaskState::Running;
            self.run(task, true).await;
        } else {
            for f in task.stack.iter().rev() {
                self.emit_step(f, "cancelled");
            }
            let msg = format!("已取消 {cap}，任务 #{id} 的计划中止");
            self.say(&task.intent, ExpressionKind::Report, msg.clone())
                .await;
            task.answer = Some(msg);
            self.finish(task, TaskState::Cancelled);
        }
    }

    fn finish(&mut self, mut task: Task, state: TaskState) {
        task.state = state;
        self.save(&mut task);
        self.log(AuditEvent::TaskState {
            task: task.id,
            intent: task.intent.id,
            state: format!("{state:?}").to_lowercase(),
        });
        self.emit(json!({"type": "done", "intent": task.intent.id, "task": task.id}));
        self.recent.push(task);
        if self.recent.len() > KEEP_FINISHED {
            self.recent.remove(0);
        }
    }

    /// Run a task until it finishes or waits for the human. `approved` marks
    /// the top frame's current step as confirmed.
    async fn run(&mut self, mut task: Task, mut approved: bool) {
        loop {
            self.save(&mut task);
            match self.advance(&mut task, &mut approved).await {
                Flow::Continue => {}
                Flow::Delegate(child) => task.stack.push(*child),
                Flow::Paused => {
                    task.state = TaskState::Waiting;
                    self.save(&mut task);
                    self.log(AuditEvent::TaskState {
                        task: task.id,
                        intent: task.intent.id,
                        state: "waiting".into(),
                    });
                    self.waiting.insert(task.id, task);
                    return;
                }
                Flow::Finished(answer) => {
                    let done = task.stack.pop().expect("a finished frame");
                    let Some(parent) = task.stack.last_mut() else {
                        if let Some(a) = &answer {
                            self.say(&task.intent, ExpressionKind::Report, a.clone())
                                .await;
                        }
                        task.answer = answer.or_else(|| Some(done.summary()));
                        self.finish(task, TaskState::Done);
                        return;
                    };
                    // A delegate's answer is the result of the parent's call.
                    let ok = done.history.iter().any(|r| r.results.iter().any(|x| x.ok))
                        || answer.is_some();
                    let content = answer.unwrap_or_else(|| done.summary());
                    self.log(AuditEvent::AgentFinished {
                        intent: task.intent.id,
                        agent: done.agent.clone(),
                        ok,
                        summary: content.clone(),
                    });
                    parent.tainted |= done.tainted;
                    let id = parent.current().map(|s| s.id.clone()).unwrap_or_default();
                    parent.results.push(ToolResult { id, ok, content });
                    let parent = task.stack.last().expect("parent");
                    self.emit_step(parent, if ok { "done" } else { "failed" });
                    task.stack.last_mut().expect("parent").next += 1;
                }
            }
        }
    }

    /// One step of the top frame: plan a round, close a round, or run a call.
    async fn advance(&mut self, task: &mut Task, approved: &mut bool) -> Flow {
        self.sync_plugins();
        let depth = task.stack.len();
        let task_id = task.id;
        let f = task.stack.last_mut().expect("non-empty stack");

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
                s.id = format!("t{task_id}_call_{}", self.calls);
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

        let Some(spec) = self.spec_of(&step.capability) else {
            self.emit_step(f, "failed");
            let msg = format!("没有这个能力：{}", step.capability);
            self.say_for(f, ExpressionKind::Error, msg.clone()).await;
            f.fail_rest(&msg);
            return Flow::Continue;
        };
        let delegate = self.agents.by_capability(&step.capability).cloned();
        let ctx = CallContext { tainted: f.tainted };
        let human_approved = std::mem::take(approved);
        let mut decision = if delegate.is_some() && depth > 1 {
            Decision::Deny {
                reason: "子 Agent 不能再委派任务".into(),
            }
        } else if human_approved {
            Decision::Allow
        } else {
            self.policy.check(&f.agent, &spec, ctx)
        };
        if decision == Decision::Allow && !human_approved && spec.risk > RiskLevel::ReadOnly {
            if let Some(judge) = &self.judge {
                let review = Review {
                    agent: f.agent.as_str(),
                    task: &f.intent.text,
                    spec: &spec,
                    args: &step.args,
                    tainted: f.tainted,
                };
                let verdict = judge.review(&review).await;
                self.log(AuditEvent::JudgeVerdict {
                    intent: f.intent.id,
                    agent: f.agent.clone(),
                    capability: step.capability.clone(),
                    verdict: match &verdict {
                        Ok(Verdict::Allow) => "allow".into(),
                        Ok(Verdict::Flag(r)) => format!("flag: {r}"),
                        Err(e) => format!("unavailable: {e}"),
                    },
                });
                if let Ok(Verdict::Flag(reason)) = verdict {
                    decision = Decision::NeedsConfirmation {
                        reason: format!("裁判提醒：{reason}"),
                        verify_identity: false,
                    };
                }
            }
        }
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
                let pin_required = verify_identity && self.pin_required();
                let id_note = match (verify_identity, pin_required) {
                    (true, true) => "（需要输入 PIN）",
                    (true, false) => {
                        "（尚未设置 PIN，任何人都能确认；建议运行 agentosd --set-pin）"
                    }
                    _ => "",
                };
                let who = if f.is_butler() {
                    String::new()
                } else {
                    format!("{}请求执行 ", f.name)
                };
                let text = format!(
                    "任务 #{task_id}：{who}{}：{}。原因：{reason}{id_note}。回复 y 确认，n 取消。",
                    step.capability, spec.description
                );
                let event = json!({
                    "type": "confirm",
                    "intent": f.intent.id,
                    "task": task_id,
                    "plan": f.plan_key(),
                    "agent": f.agent,
                    "agent_name": f.name,
                    "index": f.next,
                    "capability": step.capability.to_string(),
                    "description": spec.description,
                    "risk": risk_label(spec.risk),
                    "reason": reason,
                    "verify_identity": verify_identity,
                    "pin_required": pin_required,
                    "args": step.args,
                });
                self.emit_step(f, "confirm");
                self.emit(event.clone());
                // Asked by the butler for everyone: the human only talks to it.
                self.say(&f.intent, ExpressionKind::AskConfirm, text.clone())
                    .await;
                task.ask = Some((text, event));
                task.verify_identity = verify_identity;
                return Flow::Paused;
            }
        }

        // Delegation: start the agent on its task.
        if let Some(m) = delegate {
            let task_text = step
                .args
                .get("task")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default()
                .to_string();
            if task_text.is_empty() {
                self.emit_step(f, "failed");
                f.fail_rest("缺少 task：要交给它做什么？");
                return Flow::Continue;
            }
            let child = self.start_delegate(f, &m, task_text);
            self.emit_step(f, "running");
            self.say_for(
                f,
                ExpressionKind::Plan,
                format!("交给{}：{}", m.name, child.intent.text),
            )
            .await;
            return Flow::Delegate(Box::new(child));
        }

        self.emit_step(f, "running");
        let mut untrusted = false;
        let result = if let Some(r) = self.syscall(task_id, &step) {
            r
        } else {
            let cap = self
                .registry
                .get(&step.capability)
                .cloned()
                .expect("spec_of found it");
            untrusted = cap.untrusted_output();
            // Commit the intent to act before acting, so a crash mid-call is
            // recognised on recovery instead of silently re-run.
            f.started = Some(step.id.clone());
            self.save(task);
            let r = cap.invoke(step.args.clone()).await;
            r.map_err(|e| e.to_string())
        };
        let f = task.stack.last_mut().expect("non-empty stack");
        f.started = None;
        // Whatever the agent does after reading outside content is suspect,
        // for the rest of this task (taint never goes away within a frame,
        // and a delegate's taint flows back to its parent).
        if untrusted && result.is_ok() && !f.tainted {
            f.tainted = true;
            self.emit(json!({
                "type": "tainted",
                "task": task_id,
                "intent": f.intent.id,
                "capability": step.capability.to_string(),
            }));
        }
        let (ok, value) = match &result {
            Ok(v) => (true, v.clone()),
            Err(e) => (false, Value::String(e.clone())),
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
                    next.id = format!("t{task_id}_call_{}", self.calls);
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

    /// Calls handled by the butler itself: the task table.
    fn syscall(&mut self, current: u64, step: &Step) -> Option<Result<Value, String>> {
        match step.capability.as_str() {
            "tasks.list" => {
                let mut lines = vec![format!("#{current} 进行中（就是这个任务）")];
                lines.extend(self.waiting.values().map(Task::line));
                let skip = self.recent.len().saturating_sub(5);
                lines.extend(self.recent[skip..].iter().rev().map(Task::line));
                Some(Ok(json!({
                    "waiting": self.waiting_tasks(),
                    "message": format!("任务：\n{}", lines.join("\n")),
                })))
            }
            "tasks.cancel" => {
                let Some(id) = step.args.get("task").and_then(Value::as_u64) else {
                    return Some(Err("缺少任务编号 task".into()));
                };
                if id == current {
                    return Some(Err("不能取消正在执行的这个任务".into()));
                }
                let Some(mut t) = self.waiting.remove(&id) else {
                    return Some(Err(format!("没有等待中的任务 #{id}")));
                };
                for f in t.stack.iter().rev() {
                    self.emit_step(f, "cancelled");
                }
                t.ask = None;
                t.answer = Some("被另一个任务取消".into());
                self.finish(t, TaskState::Cancelled);
                Some(Ok(json!({"message": format!("已取消任务 #{id}")})))
            }
            _ => None,
        }
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
        let mut system =
            crate::planner::sub_agent_prompt(&m.name, &m.description, m.instructions.as_deref());
        if let Some(b) = self.board.as_ref().and_then(|b| b.summary(1000)) {
            system.push_str("\n\n共享黑板（");
            system.push_str(&b);
            system.push('）');
        }
        Frame {
            task: parent.task,
            agent,
            name: m.name.clone(),
            system,
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
            started: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_may_name_a_task() {
        assert_eq!(split_task_ref("y #12"), (Some(12), "y ".into()));
        assert_eq!(split_task_ref("确认"), (None, "确认".into()));
        assert!(is_yes("确认！"));
        assert!(is_no("算了。"));
        assert!(!is_no("算了吧，记一下牛奶"));
    }
}
