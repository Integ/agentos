//! The butler executes durable tasks. Control commands never become model input.
//!
//! Planning is a loop: the butler plans a round of capability calls, runs
//! them one by one through the policy engine, and hands the results back to
//! the planner, which plans the next round or answers. A step may be delegated
//! to another agent, whose own loop runs on the same task with narrower
//! grants. Every step is recorded in the durable task before and after it
//! runs, so clients see real progress and a restart never guesses.
//!
//! What is durable is the task's facts (steps, results, state). The planner
//! conversation of a task that is waiting for approval is kept in memory: a
//! restart interrupts unfinished tasks rather than resuming them, so there is
//! nothing to restore it for.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Read};
use std::sync::{Arc, Mutex};

use agentos_audit::{AuditEvent, AuditLog, IntentRecord};
use agentos_core::{
    now_ms, AgentId, Capability, CapabilityName, CapabilitySpec, Expression, ExpressionKind,
    Intent, IntentId, RiskLevel, Urgency,
};
use agentos_planner::{
    sub_agent_prompt, Conversation, Plan, Planner, Round, Step, ToolResult, SYSTEM_PROMPT,
};
use agentos_policy::{CallContext, Decision, PolicyEngine};
use agentos_runtime::{Confirmation, StepState, Task, TaskId, TaskState, TaskStep, TaskStore};
use agentos_services::agents::{AgentDirectory, AgentManifest};
use agentos_services::blackboard::Blackboard;
use agentos_services::expression::ExpressionHub;
use agentos_services::identity::PinVault;
use agentos_services::judge::{Judge, Review, Verdict};
use agentos_services::plugin_store::PluginManager;
use agentos_services::registry::CapabilityRegistry;
use serde_json::Value;

const CONFIRMATION_TTL_MS: u64 = 5 * 60 * 1_000;
/// Planning rounds the butler gets for one task.
pub const BUTLER_MAX_ROUNDS: u32 = 8;
/// Finished tasks the planner is reminded of.
const MEMORY_TASKS: usize = 8;

fn confirmation_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// What clients are told the system can do, kept current as plugins come and
/// go. The butler owns the registry; this is its published view.
#[derive(Default)]
pub struct CapabilityView {
    /// Everything the butler may use, including agents it can delegate to.
    pub specs: Vec<CapabilitySpec>,
    /// Capabilities a client may query directly: read-only, and with output
    /// the user controls. Untrusted output would need a task to carry its
    /// taint, so such a capability is never served outside one.
    pub readable: BTreeMap<CapabilityName, Arc<dyn Capability>>,
}

/// One agent's work on a task: the unit of the call stack. The butler's frame
/// is at the bottom; a delegate's frame sits above it while it works.
struct Frame {
    agent: AgentId,
    /// Display name of a delegate; `None` for the butler.
    name: Option<String>,
    system: String,
    /// The human's intent for the butler; the handed-over task for a delegate.
    intent: Intent,
    specs: Vec<CapabilitySpec>,
    history: Vec<Round>,
    /// The round being executed, and the ids of its steps in the durable task.
    plan: Option<Plan>,
    keys: Vec<String>,
    next: usize,
    results: Vec<ToolResult>,
    rounds: u32,
    max_rounds: u32,
    tainted: bool,
    /// Where this frame's next steps are recorded in the task: right after
    /// the delegating step for a delegate, at the end for the butler.
    insert_at: Option<usize>,
}

impl Frame {
    fn is_butler(&self) -> bool {
        self.name.is_none()
    }

    fn current(&self) -> Option<(&Step, &str)> {
        let step = self.plan.as_ref()?.steps.get(self.next)?;
        Some((step, self.keys.get(self.next)?.as_str()))
    }

    /// The last round this frame ran left a call that did not succeed.
    fn ended_on_failure(&self) -> Option<&str> {
        self.history
            .last()?
            .results
            .iter()
            .find(|result| !result.ok)
            .map(|result| result.content.as_str())
    }

    /// What a delegate hands back when its planner gave no closing words.
    fn summary(&self) -> String {
        let done: Vec<String> = self
            .history
            .iter()
            .flat_map(|round| round.calls.iter().zip(&round.results))
            .filter(|(_, result)| result.ok)
            .map(|(call, _)| call.capability.to_string())
            .collect();
        if done.is_empty() {
            "没有完成任何步骤".into()
        } else {
            format!("已完成：{}", done.join("、"))
        }
    }
}

enum Flow {
    Continue,
    /// The top frame is done; its answer, if any.
    Finished(Option<String>),
    /// The top frame's planner could not act on its request at all.
    Unhandled(String),
    /// The top frame cannot continue: its planner failed or its plan is invalid.
    Failed(String),
    /// Waiting for the human; the frames are kept until the approval arrives.
    Paused,
    /// The top frame handed its current call to a new agent.
    Delegate(Box<Frame>),
    /// The task reached a terminal state from outside (it was cancelled).
    Stop,
}

pub struct Butler {
    id: AgentId,
    registry: CapabilityRegistry,
    policy: PolicyEngine,
    planner: Box<dyn Planner>,
    audit: Arc<AuditLog>,
    expr: ExpressionHub,
    tasks: Arc<TaskStore>,
    agents: AgentDirectory,
    board: Option<Blackboard>,
    judge: Option<Arc<dyn Judge>>,
    pins: Option<Arc<PinVault>>,
    plugins: Option<Arc<PluginManager>>,
    plugin_generation: u64,
    plugin_names: Vec<CapabilityName>,
    view: Arc<Mutex<CapabilityView>>,
    /// Call stacks of tasks waiting for approval.
    suspended: HashMap<TaskId, Vec<Frame>>,
    /// Source of step ids, unique within this process and so within a task.
    calls: u64,
}

impl Butler {
    /// Embedders/tests may use an in-memory store. The daemon uses `with_store`.
    pub fn new(
        id: AgentId,
        registry: CapabilityRegistry,
        policy: PolicyEngine,
        planner: Box<dyn Planner>,
        audit: Arc<AuditLog>,
        expr: ExpressionHub,
    ) -> Self {
        Self::with_store(
            id,
            registry,
            policy,
            planner,
            audit,
            expr,
            Arc::new(TaskStore::memory()),
        )
    }

    pub fn with_store(
        id: AgentId,
        registry: CapabilityRegistry,
        policy: PolicyEngine,
        planner: Box<dyn Planner>,
        audit: Arc<AuditLog>,
        expr: ExpressionHub,
        tasks: Arc<TaskStore>,
    ) -> Self {
        Self {
            id,
            registry,
            policy,
            planner,
            audit,
            expr,
            tasks,
            agents: AgentDirectory::new(),
            board: None,
            judge: None,
            pins: None,
            plugins: None,
            plugin_generation: 0,
            plugin_names: Vec::new(),
            view: Arc::default(),
            suspended: HashMap::new(),
            calls: 0,
        }
    }

    /// The published capability view, shared with the control service.
    pub fn view(&self) -> Arc<Mutex<CapabilityView>> {
        self.publish();
        self.view.clone()
    }

    fn publish(&self) {
        let specs = self.capabilities();
        let readable = specs
            .iter()
            .filter(|spec| spec.risk == RiskLevel::ReadOnly)
            .filter_map(|spec| self.registry.get(&spec.name))
            .filter(|capability| !capability.untrusted_output())
            .map(|capability| (capability.spec().name.clone(), capability.clone()))
            .collect();
        *self.view.lock().unwrap_or_else(|p| p.into_inner()) = CapabilityView { specs, readable };
    }

    /// Agents the butler may delegate to. Each is offered to the planner as
    /// the capability `agent.<id>`, which the butler is granted here.
    pub fn with_agents(mut self, agents: AgentDirectory) -> Self {
        for manifest in agents.all() {
            self.policy.grant(&self.id, &manifest.delegate_name());
        }
        self.agents = agents;
        self
    }

    /// The shared blackboard; its content is part of every agent's context.
    pub fn with_blackboard(mut self, board: Blackboard) -> Self {
        self.board = Some(board);
        self
    }

    /// A judge that may turn an allowed step into one that needs approval.
    pub fn with_judge(mut self, judge: Arc<dyn Judge>) -> Self {
        self.judge = Some(judge);
        self
    }

    /// The PIN that vouches for irreversible steps. Without a PIN that is
    /// set, such steps are refused.
    pub fn with_pin_vault(mut self, pins: Arc<PinVault>) -> Self {
        self.pins = Some(pins);
        self
    }

    /// Installed plugins become capabilities, and stop being ones when they
    /// are disabled or rolled back.
    pub fn with_plugins(mut self, plugins: Arc<PluginManager>) -> Self {
        plugins.reserve(self.registry.specs().into_iter().map(|spec| spec.name));
        self.plugins = Some(plugins);
        self.plugin_generation = u64::MAX;
        self.sync_plugins();
        self
    }

    pub fn id(&self) -> &AgentId {
        &self.id
    }

    pub fn tasks(&self) -> &Arc<TaskStore> {
        &self.tasks
    }

    pub fn policy_mut(&mut self) -> &mut PolicyEngine {
        &mut self.policy
    }

    /// What the butler may currently use, including agents it can delegate to.
    pub fn capabilities(&self) -> Vec<CapabilitySpec> {
        let mut specs: Vec<_> = self
            .registry
            .specs()
            .into_iter()
            .filter(|spec| self.policy.is_granted(&self.id, &spec.name))
            .collect();
        specs.extend(
            self.agents
                .all()
                .map(AgentManifest::delegate_spec)
                .filter(|spec| self.policy.is_granted(&self.id, &spec.name)),
        );
        specs
    }

    pub fn awaiting_confirmation(&self) -> bool {
        self.tasks
            .list()
            .iter()
            .any(|t| t.state == TaskState::AwaitingConfirmation)
    }

    /// Bring the registry and grants in line with the active plugins.
    fn sync_plugins(&mut self) {
        let Some(plugins) = self.plugins.clone() else {
            return;
        };
        let generation = plugins.generation();
        if generation == self.plugin_generation {
            return;
        }
        self.plugin_generation = generation;
        for name in self.plugin_names.drain(..) {
            self.registry.unregister(&name);
            self.policy.revoke(&self.id, &name);
        }
        for capability in plugins.active() {
            let name = capability.spec().name.clone();
            if self.registry.get(&name).is_some() {
                // Never let a plugin shadow a built-in capability.
                continue;
            }
            self.registry.register(capability);
            self.policy.grant(&self.id, &name);
            self.plugin_names.push(name);
        }
        self.publish();
    }

    async fn say(&self, task: &Task, kind: ExpressionKind, text: impl Into<String>) {
        self.expr
            .express(Expression {
                from: self.id.clone(),
                intent: Some(task.intent.id),
                urgency: if kind == ExpressionKind::AskConfirm {
                    Urgency::High
                } else {
                    task.intent.urgency
                },
                kind,
                text: format!("[{}] {}", task.id, text.into()),
            })
            .await;
    }

    /// What `frame` says, attributed to it when it is not the butler: the
    /// human only ever talks to the butler.
    async fn say_for(&self, task: &Task, frame: &Frame, kind: ExpressionKind, text: String) {
        let text = match &frame.name {
            Some(name) => format!("〔{name}〕{text}"),
            None => text,
        };
        self.say(task, kind, text).await;
    }

    fn log_state(&self, task: &Task) -> io::Result<()> {
        self.audit.append(AuditEvent::TaskStateChanged {
            intent: task.intent.id,
            state: serde_json::to_value(task.state)
                .map_err(io::Error::other)?
                .as_str()
                .unwrap_or("unknown")
                .into(),
            message: task.message.clone(),
        })?;
        Ok(())
    }

    fn transition(
        &self,
        id: TaskId,
        state: TaskState,
        message: Option<String>,
    ) -> io::Result<Task> {
        let task = self.tasks.update(id, |t| {
            // Never overwrite cancellation when an in-flight operation returns.
            if !t.state.is_terminal() {
                t.state = state;
                t.message = message;
                t.confirmation = None;
            }
        })?;
        self.log_state(&task)?;
        Ok(task)
    }

    /// What the planner should know beyond the request: recent tasks, tasks
    /// waiting for approval, and the blackboard.
    fn context(&self, current: TaskId) -> String {
        let mut text = SYSTEM_PROMPT.to_string();
        let line = |task: &Task| {
            let mut line = format!(
                "{} {}：{}",
                task.id,
                crate::control::state_label(task.state),
                task.intent.text.chars().take(200).collect::<String>()
            );
            if let Some(message) = &task.message {
                line.push_str(" → ");
                line.extend(message.chars().take(120));
            }
            line
        };
        let (recent, _, _) = self.tasks.page(Some(current), MEMORY_TASKS * 2, None);
        let finished: Vec<&Task> = recent
            .iter()
            .filter(|task| task.state.is_terminal())
            .take(MEMORY_TASKS)
            .collect();
        if !finished.is_empty() {
            text.push_str("\n\n最近结束的任务（旧的在前，供参考，不需要重做）：");
            for task in finished.iter().rev() {
                text.push_str("\n- ");
                text.push_str(&line(task));
            }
        }
        let waiting: Vec<&Task> = recent
            .iter()
            .filter(|task| task.state == TaskState::AwaitingConfirmation)
            .collect();
        if !waiting.is_empty() {
            text.push_str("\n\n正在等待用户批准的任务：");
            for task in waiting.iter().rev() {
                text.push_str("\n- ");
                text.push_str(&line(task));
            }
        }
        if let Some(board) = self.board.as_ref().and_then(|board| board.summary(1500)) {
            text.push_str("\n\n共享黑板（");
            text.push_str(&board);
            text.push('）');
        }
        text
    }

    /// New input never implicitly approves an older task, even if it is "y".
    pub async fn handle(&mut self, intent: Intent) {
        match self.tasks.create(intent) {
            Ok(task) => self.execute(task.id).await,
            Err(e) => eprintln!("无法保存任务：{e}"),
        }
    }

    pub async fn execute(&mut self, id: TaskId) {
        if let Err(e) = self.execute_inner(id).await {
            self.storage_error(id, e).await;
        }
    }

    async fn execute_inner(&mut self, id: TaskId) -> io::Result<()> {
        // Stacks of tasks cancelled while they waited are no longer needed.
        let tasks = self.tasks.clone();
        self.suspended
            .retain(|id, _| tasks.get(*id).is_some_and(|t| !t.state.is_terminal()));
        let Some(task) = self.tasks.get(id) else {
            return Ok(());
        };
        if task.state != TaskState::Queued {
            return Ok(());
        }
        self.audit.append(AuditEvent::IntentReceived {
            intent: Box::new(IntentRecord::from(&task.intent)),
        })?;
        let task = self.transition(id, TaskState::Planning, None)?;
        if task.state == TaskState::Cancelled {
            return Ok(());
        }
        self.sync_plugins();
        let frame = Frame {
            agent: self.id.clone(),
            name: None,
            system: self.context(id),
            intent: task.intent.clone(),
            specs: self.capabilities(),
            history: Vec::new(),
            plan: None,
            keys: Vec::new(),
            next: 0,
            results: Vec::new(),
            rounds: 0,
            max_rounds: BUTLER_MAX_ROUNDS,
            tainted: false,
            insert_at: None,
        };
        self.drive(id, vec![frame], None).await
    }

    /// Trusted in-process compatibility entry point. IPC callers must supply
    /// the token from the concrete confirmation via `approve_with_token`.
    pub async fn approve(&mut self, id: TaskId, step: usize, responder: &Intent) {
        let token = self
            .tasks
            .get(id)
            .and_then(|t| t.confirmation)
            .map(|c| c.token)
            .unwrap_or_default();
        self.approve_with_token(id, step, responder, &token).await;
    }

    /// A token binds approval to one concrete prompt across client reconnects.
    /// UID/source matching is still required; identity for an irreversible
    /// step is proven separately, by the PIN.
    pub async fn approve_with_token(
        &mut self,
        id: TaskId,
        step: usize,
        responder: &Intent,
        token: &str,
    ) {
        if let Err(e) = self.approve_inner(id, step, responder, token).await {
            self.storage_error(id, e).await;
        }
    }

    async fn approve_inner(
        &mut self,
        id: TaskId,
        step: usize,
        responder: &Intent,
        token: &str,
    ) -> io::Result<()> {
        let Some(task) = self.tasks.get(id) else {
            return Ok(());
        };
        let valid =
            crate::control::approval_matches_with_token(&task, step, responder, now_ms(), token);
        if !valid {
            self.say(
                &task,
                ExpressionKind::Error,
                "批准无效：请核对任务、步骤、输入来源和五分钟有效期",
            )
            .await;
            return Ok(());
        }
        let Some(stack) = self.suspended.remove(&id) else {
            // Cannot happen within one run of the service; never guess.
            let message = "无法继续：等待批准时的执行现场已丢失，任务已停止";
            let task = self.transition(id, TaskState::Failed, Some(message.into()))?;
            self.say(&task, ExpressionKind::Error, message).await;
            return Ok(());
        };
        self.audit.append(AuditEvent::HumanConfirmation {
            intent: task.intent.id,
            capability: task.steps[step].capability.clone(),
            approved: true,
        })?;
        let key = task.steps[step].id.clone();
        let task = self.transition(id, TaskState::Running, None)?;
        if task.state != TaskState::Cancelled {
            self.drive(id, stack, Some(key)).await?;
        }
        Ok(())
    }

    /// Run a task until it finishes or waits for the human. `approved` is the
    /// id of the step the human just approved.
    async fn drive(
        &mut self,
        id: TaskId,
        mut stack: Vec<Frame>,
        mut approved: Option<String>,
    ) -> io::Result<()> {
        loop {
            let flow = self.advance(id, &mut stack, &mut approved).await?;
            let (answer, outcome) = match flow {
                Flow::Continue => continue,
                Flow::Stop => return Ok(()),
                Flow::Paused => {
                    self.suspended.insert(id, stack);
                    return Ok(());
                }
                Flow::Delegate(child) => {
                    stack.push(*child);
                    continue;
                }
                Flow::Finished(answer) => (answer, TaskState::Succeeded),
                Flow::Unhandled(reason) => (Some(reason), TaskState::Unhandled),
                Flow::Failed(error) => (Some(error), TaskState::Failed),
            };
            let done = stack.pop().expect("a finished frame");
            // Ending on a call that did not succeed is not success, whatever
            // was said about it: the last thing attempted did not happen.
            let failure = done.ended_on_failure().map(str::to_string);
            let outcome = match (&failure, outcome) {
                (Some(_), TaskState::Succeeded) => TaskState::Failed,
                (_, outcome) => outcome,
            };
            let Some(parent) = stack.last_mut() else {
                return self.finish(id, &done, answer, outcome, failure).await;
            };
            // A delegate's answer is the result of the parent's call.
            let ok = outcome == TaskState::Succeeded
                && (answer.is_some()
                    || done
                        .history
                        .iter()
                        .any(|round| round.results.iter().any(|result| result.ok)));
            let content = answer.or(failure).unwrap_or_else(|| done.summary());
            self.audit.append(AuditEvent::AgentFinished {
                intent: IntentId(id.0),
                agent: done.agent.clone(),
                ok,
                summary: content.clone(),
            })?;
            parent.tainted |= done.tainted;
            let (call, key) = parent
                .current()
                .map(|(step, key)| (step.id.clone(), key.to_string()))
                .expect("the delegating step");
            parent.results.push(ToolResult {
                id: call,
                ok,
                content: content.clone(),
            });
            parent.next += 1;
            let tainted = parent.tainted;
            let task = self.tasks.update(id, |t| {
                if let Some(step) = t.steps.iter_mut().find(|step| step.id == key) {
                    step.state = if ok {
                        StepState::Succeeded
                    } else {
                        StepState::Failed
                    };
                    step.result = Some(Value::String(content.clone()));
                }
                t.tainted |= tainted;
            })?;
            if task.state.is_terminal() {
                return Ok(());
            }
        }
    }

    /// The butler's own frame ended: settle the task.
    async fn finish(
        &mut self,
        id: TaskId,
        done: &Frame,
        answer: Option<String>,
        outcome: TaskState,
        failure: Option<String>,
    ) -> io::Result<()> {
        match outcome {
            TaskState::Succeeded => {
                let message = answer.clone().unwrap_or_else(|| "任务已完成".into());
                let task = self.transition(id, TaskState::Succeeded, Some(message))?;
                // Each step already reported its own result; only a closing
                // answer from the planner is new.
                if let (Some(answer), TaskState::Succeeded) = (answer, task.state) {
                    self.say(&task, ExpressionKind::Report, answer).await;
                }
            }
            TaskState::Unhandled => {
                let reply = answer.unwrap_or_else(|| "我还不会做这件事".into());
                let task = self.transition(id, TaskState::Unhandled, Some(reply.clone()))?;
                self.say(&task, ExpressionKind::Report, reply).await;
            }
            _ => {
                // A planner error has not been said yet; a step's failure has.
                let already_said = answer.is_none() && failure.is_some();
                let message = answer.or(failure).unwrap_or_else(|| done.summary());
                let task = self.transition(id, TaskState::Failed, Some(message.clone()))?;
                if !already_said {
                    self.say(&task, ExpressionKind::Error, message).await;
                }
            }
        }
        Ok(())
    }

    /// Record `message` as the failure of the frame's current step and skip
    /// the rest of the round: later steps may depend on it. The planner sees
    /// the failure and decides what next.
    fn fail_step(&self, id: TaskId, frame: &mut Frame, message: &str) -> io::Result<Task> {
        let Some(plan) = &frame.plan else {
            return self.tasks.update(id, |_| {});
        };
        let failed = frame.keys[frame.next].clone();
        let skipped: Vec<String> = frame.keys[frame.next + 1..].to_vec();
        for (offset, step) in plan.steps.iter().enumerate().skip(frame.next) {
            frame.results.push(ToolResult {
                id: step.id.clone(),
                ok: false,
                content: if offset == frame.next {
                    message.to_string()
                } else {
                    "未执行：前面的步骤没有成功".into()
                },
            });
        }
        frame.next = plan.steps.len();
        self.tasks.update(id, |t| {
            for step in &mut t.steps {
                if step.id == failed {
                    step.state = StepState::Failed;
                    step.result = Some(Value::String(message.to_string()));
                } else if skipped.contains(&step.id) {
                    step.state = StepState::Skipped;
                }
            }
        })
    }

    /// One step of the top frame: plan a round, close a round, or run a call.
    async fn advance(
        &mut self,
        id: TaskId,
        stack: &mut [Frame],
        approved: &mut Option<String>,
    ) -> io::Result<Flow> {
        let depth = stack.len();
        let frame = stack.last_mut().expect("non-empty stack");
        let Some(task) = self.tasks.get(id) else {
            return Ok(Flow::Stop);
        };
        if task.state.is_terminal() {
            return Ok(Flow::Stop);
        }

        // Plan the next round.
        if frame.plan.is_none() {
            if frame.rounds >= frame.max_rounds {
                let message = format!("已经连续执行了 {} 轮，先停在这里。", frame.max_rounds);
                return Ok(Flow::Finished(Some(message)));
            }
            self.sync_plugins();
            if frame.is_butler() {
                frame.specs = self.capabilities();
            }
            let mut plan = self
                .planner
                .plan_turn(&Conversation {
                    system: &frame.system,
                    intent: &frame.intent,
                    history: &frame.history,
                    available: &frame.specs,
                })
                .await;
            frame.rounds += 1;
            if self.tasks.is_cancelled(id) {
                return Ok(Flow::Stop);
            }
            if let Some(error) = plan.error.take() {
                return Ok(Flow::Failed(error));
            }
            // Reject the whole round before running an invalid subset of it.
            for step in &plan.steps {
                let offered = frame.specs.iter().any(|spec| spec.name == step.capability);
                if !offered
                    || !self.policy.is_granted(&frame.agent, &step.capability)
                    || !step.args.is_object()
                {
                    return Ok(Flow::Failed(format!(
                        "计划无效：{} 未授权、不存在或参数不是对象",
                        step.capability
                    )));
                }
            }
            self.audit.append(AuditEvent::PlanCreated {
                intent: task.intent.id,
                agent: frame.agent.clone(),
                steps: plan
                    .steps
                    .iter()
                    .map(|s| s.capability.to_string())
                    .collect(),
            })?;
            if plan.steps.is_empty() {
                // Nothing ran yet and the planner either said it has no way to
                // act or said nothing at all: not understood, not done.
                if frame.history.is_empty() && (plan.unhandled || plan.reply.is_none()) {
                    return Ok(Flow::Unhandled(
                        plan.reply.unwrap_or_else(|| "我还不会做这件事".into()),
                    ));
                }
                return Ok(Flow::Finished(plan.reply));
            }
            frame.tainted |= plan.tainted;
            let keys: Vec<String> = plan
                .steps
                .iter()
                .map(|_| {
                    self.calls += 1;
                    format!("call-{}", self.calls)
                })
                .collect();
            for (step, key) in plan.steps.iter_mut().zip(&keys) {
                if step.id.is_empty() {
                    step.id = key.clone();
                }
            }
            let agent = (!frame.is_butler()).then(|| frame.agent.clone());
            let recorded: Vec<TaskStep> = plan
                .steps
                .iter()
                .zip(&keys)
                .map(|(step, key)| TaskStep {
                    id: key.clone(),
                    agent: agent.clone(),
                    ..TaskStep::pending(step.capability.clone(), step.args.clone())
                })
                .collect();
            let first_round = task.state == TaskState::Planning;
            let (tainted, at) = (frame.tainted, frame.insert_at);
            let task = self.tasks.update(id, |t| {
                if !t.state.is_terminal() {
                    let at = at.unwrap_or(t.steps.len()).min(t.steps.len());
                    t.steps.splice(at..at, recorded.clone());
                    t.tainted |= tainted;
                    t.state = TaskState::Running;
                }
            })?;
            if task.state.is_terminal() {
                return Ok(Flow::Stop);
            }
            if let Some(at) = &mut frame.insert_at {
                *at += keys.len();
            }
            if first_round {
                self.log_state(&task)?;
            }
            if let (Some(reply), true) = (&plan.reply, frame.is_butler()) {
                self.say(&task, ExpressionKind::Plan, reply.clone()).await;
            }
            let outline = plan
                .steps
                .iter()
                .enumerate()
                .map(|(i, s)| format!("{}. {} {}", i + 1, s.capability, s.args))
                .collect::<Vec<_>>()
                .join("；");
            self.say_for(&task, frame, ExpressionKind::Plan, outline)
                .await;
            frame.plan = Some(plan);
            frame.keys = keys;
            frame.next = 0;
            frame.results.clear();
            return Ok(Flow::Continue);
        }

        // Close a finished round and hand the results back to the planner.
        let Some((step, key)) = frame
            .current()
            .map(|(step, key)| (step.clone(), key.to_string()))
        else {
            let plan = frame.plan.take().expect("checked above");
            frame.keys.clear();
            frame.history.push(Round {
                text: plan.reply,
                calls: plan.steps,
                results: std::mem::take(&mut frame.results),
            });
            return Ok(Flow::Continue);
        };

        let delegate = self.agents.by_capability(&step.capability).cloned();
        let capability = self.registry.get(&step.capability).cloned();
        let spec = match (&delegate, &capability) {
            (Some(manifest), _) => manifest.delegate_spec(),
            (None, Some(capability)) => capability.spec().clone(),
            (None, None) => {
                let message = format!("能力已卸载：{}", step.capability);
                let task = self.fail_step(id, frame, &message)?;
                self.say_for(&task, frame, ExpressionKind::Error, message)
                    .await;
                return Ok(Flow::Continue);
            }
        };
        let was_approved = approved.take().as_deref() == Some(key.as_str());
        let mut decision = if delegate.is_some() && depth > 1 {
            Decision::Deny {
                reason: "子 Agent 不能再委派任务".into(),
            }
        } else {
            // Checked again after an approval: a grant revoked while the task
            // waited must not be bypassed by the approval.
            self.policy.check(
                &frame.agent,
                &spec,
                CallContext {
                    tainted: frame.tainted,
                },
            )
        };
        if decision == Decision::Allow && !was_approved && spec.risk > RiskLevel::ReadOnly {
            if let Some(judge) = &self.judge {
                let verdict = judge
                    .review(&Review {
                        agent: frame.agent.as_str(),
                        task: &frame.intent.text,
                        spec: &spec,
                        args: &step.args,
                        tainted: frame.tainted,
                    })
                    .await;
                self.audit.append(AuditEvent::JudgeVerdict {
                    intent: task.intent.id,
                    agent: frame.agent.clone(),
                    capability: step.capability.clone(),
                    verdict: match &verdict {
                        Ok(Verdict::Allow) => "allow".into(),
                        Ok(Verdict::Flag(reason)) => format!("flag: {reason}"),
                        Err(error) => format!("unavailable: {error}"),
                    },
                })?;
                if let Ok(Verdict::Flag(reason)) = verdict {
                    decision = Decision::NeedsConfirmation {
                        reason: format!("裁判提醒：{reason}"),
                        verify_identity: false,
                    };
                }
            }
        }
        self.audit.append(AuditEvent::PolicyDecision {
            intent: task.intent.id,
            agent: frame.agent.clone(),
            capability: step.capability.clone(),
            decision: format!("{decision:?}"),
        })?;
        match decision {
            Decision::Allow => {}
            Decision::Deny { reason } => {
                let message = format!("被策略拒绝：{reason}");
                let task = self.fail_step(id, frame, &message)?;
                self.say_for(&task, frame, ExpressionKind::Error, message)
                    .await;
                return Ok(Flow::Continue);
            }
            Decision::NeedsConfirmation {
                reason,
                verify_identity,
            } => {
                // An irreversible step needs proof of who approves it. The
                // PIN is that proof; without one the step is refused.
                let pin_set = self.pins.as_ref().is_some_and(|pins| pins.is_set());
                let refusal = if verify_identity && !pin_set {
                    Some("此操作需要可信身份核验；尚未设置 PIN，已拒绝执行")
                } else if was_approved
                    && verify_identity
                    && !self.pins.as_ref().is_some_and(|pins| pins.take_grant(id.0))
                {
                    Some("此操作需要 PIN 核验身份；批准时没有提供有效的 PIN，已拒绝执行")
                } else {
                    None
                };
                if let Some(message) = refusal {
                    let task = self.fail_step(id, frame, message)?;
                    self.say_for(&task, frame, ExpressionKind::Error, message.into())
                        .await;
                    return Ok(Flow::Continue);
                }
                if !was_approved {
                    let token = confirmation_token()?;
                    let task = self.tasks.update(id, |t| {
                        if t.state.is_terminal() {
                            return;
                        }
                        if let Some(index) = t.steps.iter().position(|step| step.id == key) {
                            t.state = TaskState::AwaitingConfirmation;
                            t.message = Some(reason.clone());
                            t.confirmation = Some(Confirmation {
                                step: index,
                                expires_at_ms: now_ms().saturating_add(CONFIRMATION_TTL_MS),
                                token,
                                verify_identity,
                            });
                        }
                    })?;
                    self.log_state(&task)?;
                    if task.state.is_terminal() {
                        return Ok(Flow::Stop);
                    }
                    let number = task.confirmation.as_ref().map_or(0, |c| c.step + 1);
                    let identity = if verify_identity {
                        "此操作不可逆，批准时需要输入 PIN。"
                    } else {
                        ""
                    };
                    self.say_for(
                        &task,
                        frame,
                        ExpressionKind::AskConfirm,
                        format!(
                            "步骤 {number}：{} {}。原因：{reason}。{identity}五分钟内输入 /approve {id} {number} 批准，或 /cancel {id} 取消。可继续提交其他任务。",
                            step.capability, step.args,
                        ),
                    )
                    .await;
                    return Ok(Flow::Paused);
                }
            }
        }

        // Delegation: start the agent on its task.
        if let Some(manifest) = delegate {
            let handed = step
                .args
                .get("task")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default()
                .to_string();
            if handed.is_empty() {
                let message = "缺少 task：要交给它做什么？";
                let task = self.fail_step(id, frame, message)?;
                self.say_for(&task, frame, ExpressionKind::Error, message.into())
                    .await;
                return Ok(Flow::Continue);
            }
            let task = self.tasks.update(id, |t| {
                if t.state.is_terminal() {
                    return;
                }
                if let Some(step) = t.steps.iter_mut().find(|step| step.id == key) {
                    step.state = StepState::Running;
                }
            })?;
            if task.state.is_terminal() {
                return Ok(Flow::Stop);
            }
            let at = task.steps.iter().position(|step| step.id == key);
            let child = self.start_delegate(&task, frame, &manifest, handed.clone(), at)?;
            self.say_for(
                &task,
                frame,
                ExpressionKind::Plan,
                format!("交给{}：{handed}", manifest.name),
            )
            .await;
            return Ok(Flow::Delegate(Box::new(child)));
        }

        let capability = capability.expect("a non-delegate step has a capability");
        // Both records must be durable before the capability gets control.
        self.audit.append(AuditEvent::CapabilityStarted {
            intent: task.intent.id,
            agent: frame.agent.clone(),
            capability: step.capability.clone(),
            args: step.args.clone(),
        })?;
        let started = self.tasks.update(id, |t| {
            if !t.state.is_terminal() {
                t.state = TaskState::Running;
                if let Some(step) = t.steps.iter_mut().find(|step| step.id == key) {
                    step.state = StepState::Running;
                }
            }
        })?;
        if started.state.is_terminal() {
            return Ok(Flow::Stop);
        }
        let result = capability.invoke(step.args.clone()).await;
        let (ok, value) = match &result {
            Ok(v) => (true, v.clone()),
            Err(e) => (false, Value::String(e.to_string())),
        };
        self.audit.append(AuditEvent::CapabilityInvoked {
            intent: task.intent.id,
            agent: frame.agent.clone(),
            capability: step.capability.clone(),
            args: step.args.clone(),
            ok,
            result: value.clone(),
        })?;
        // Whatever an agent does after reading outside content is suspect for
        // the rest of the task, and a delegate's taint flows back to its parent.
        let untrusted = capability.untrusted_output();
        if ok && untrusted {
            frame.tainted = true;
        }
        let tainted = frame.tainted;
        // A capability describes its own result in `message`; a failure is
        // its error text as it is, not that text encoded as JSON.
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .or(value.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string());
        let task = if ok {
            frame.results.push(ToolResult {
                id: step.id.clone(),
                ok: true,
                content: value.to_string(),
            });
            // A trusted capability may ask for a next step (a proposal asks to
            // be installed). It is planned like any other and goes through
            // policy; output from outside can never do this.
            let follow_up = (!untrusted)
                .then(|| follow_up(&value))
                .flatten()
                .filter(|next| frame.specs.iter().any(|spec| spec.name == next.capability));
            let next_key = follow_up.as_ref().map(|_| {
                self.calls += 1;
                format!("call-{}", self.calls)
            });
            let agent = (!frame.is_butler()).then(|| frame.agent.clone());
            let task = self.tasks.update(id, |t| {
                t.tainted |= tainted;
                let Some(index) = t.steps.iter().position(|step| step.id == key) else {
                    return;
                };
                t.steps[index].state = StepState::Succeeded;
                t.steps[index].result = Some(value.clone());
                if let (Some(next), Some(next_key), false) =
                    (&follow_up, &next_key, t.state.is_terminal())
                {
                    t.steps.insert(
                        index + 1,
                        TaskStep {
                            id: next_key.clone(),
                            agent: agent.clone(),
                            ..TaskStep::pending(next.capability.clone(), next.args.clone())
                        },
                    );
                }
            })?;
            if let (Some(mut next), Some(next_key)) = (follow_up, next_key) {
                next.id = next_key.clone();
                let at = frame.next + 1;
                frame
                    .plan
                    .as_mut()
                    .expect("running plan")
                    .steps
                    .insert(at, next);
                frame.keys.insert(at, next_key);
                if let Some(insert_at) = &mut frame.insert_at {
                    *insert_at += 1;
                }
            }
            frame.next += 1;
            task
        } else {
            self.fail_step(id, frame, &message)?
        };
        self.say_for(
            &task,
            frame,
            if ok {
                ExpressionKind::Report
            } else {
                ExpressionKind::Error
            },
            message,
        )
        .await;
        Ok(Flow::Continue)
    }

    /// Build the delegate's frame and grant it exactly what it may use: what
    /// its manifest asks for, the delegating agent holds, and its risk ceiling
    /// allows. Never more.
    fn start_delegate(
        &mut self,
        task: &Task,
        parent: &Frame,
        manifest: &AgentManifest,
        handed: String,
        parent_step: Option<usize>,
    ) -> io::Result<Frame> {
        let holder = parent.agent.clone();
        let specs = manifest.effective(&self.registry.specs(), |name| {
            self.policy.is_granted(&holder, name)
        });
        let names: Vec<CapabilityName> = specs.iter().map(|spec| spec.name.clone()).collect();
        let agent = manifest.agent_id();
        self.policy.set_grants(&agent, names.clone());
        self.audit.append(AuditEvent::Delegated {
            intent: task.intent.id,
            from: parent.agent.clone(),
            to: agent.clone(),
            task: handed.clone(),
            capabilities: names,
        })?;
        let mut system = sub_agent_prompt(
            &manifest.name,
            &manifest.description,
            manifest.instructions.as_deref(),
        );
        if let Some(board) = self.board.as_ref().and_then(|board| board.summary(1000)) {
            system.push_str("\n\n共享黑板（");
            system.push_str(&board);
            system.push('）');
        }
        Ok(Frame {
            agent,
            name: Some(manifest.name.clone()),
            system,
            intent: Intent {
                speaker: None,
                text: handed,
                ..parent.intent.clone()
            },
            specs,
            history: Vec::new(),
            plan: None,
            keys: Vec::new(),
            next: 0,
            results: Vec::new(),
            rounds: 0,
            max_rounds: manifest.max_rounds,
            tainted: parent.tainted,
            insert_at: Some(parent_step.map_or(task.steps.len(), |index| index + 1)),
        })
    }

    async fn storage_error(&mut self, id: TaskId, error: io::Error) {
        self.suspended.remove(&id);
        let message = format!("持久化或审计失败，已停止执行：{error}");
        match self.tasks.update(id, |t| {
            if !t.state.is_terminal() {
                t.state = if t.steps.iter().any(|s| s.state == StepState::Running) {
                    TaskState::Interrupted
                } else {
                    TaskState::Failed
                };
                t.confirmation = None;
                t.message = Some(message.clone());
            }
        }) {
            Ok(task) => self.say(&task, ExpressionKind::Error, message).await,
            Err(e) => eprintln!("{id}：{message}；无法保存失败状态：{e}"),
        }
    }
}

fn follow_up(value: &Value) -> Option<Step> {
    let next = value.get("follow_up")?;
    let args = next
        .get("args")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    args.is_object().then_some(())?;
    Some(Step::new(
        CapabilityName::new(next.get("capability")?.as_str()?),
        args,
    ))
}

pub fn grant_manifest(policy: &mut PolicyEngine, agent: &AgentId, names: &[CapabilityName]) {
    for n in names {
        policy.grant(agent, n);
    }
}
