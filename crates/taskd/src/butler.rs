//! The butler executes durable tasks. Control commands never become model input.

use std::io::{self, Read};
use std::sync::Arc;

use agentos_audit::durable::{AuditEvent, AuditLog, IntentRecord};
use agentos_core::{now_ms, AgentId, CapabilityName, Expression, ExpressionKind, Intent, Urgency};
use agentos_policy::{CallContext, Decision, PolicyEngine};
use agentos_runtime::{Confirmation, StepState, Task, TaskId, TaskState, TaskStep, TaskStore};
use serde_json::Value;

use crate::expression::ExpressionHub;
use crate::planner::Planner;
use crate::registry::CapabilityRegistry;

const CONFIRMATION_TTL_MS: u64 = 5 * 60 * 1_000;

fn confirmation_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub struct Butler {
    id: AgentId,
    registry: CapabilityRegistry,
    policy: PolicyEngine,
    planner: Box<dyn Planner>,
    audit: Arc<AuditLog>,
    expr: ExpressionHub,
    tasks: Arc<TaskStore>,
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
        }
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

    pub fn awaiting_confirmation(&self) -> bool {
        self.tasks
            .list()
            .iter()
            .any(|t| t.state == TaskState::AwaitingConfirmation)
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
        let specs = self
            .registry
            .specs()
            .into_iter()
            .filter(|s| self.policy.is_granted(&self.id, &s.name))
            .collect::<Vec<_>>();
        let plan = self.planner.plan(&task.intent, &specs).await;
        if self.tasks.is_cancelled(id) {
            return Ok(());
        }
        if let Some(error) = plan.error {
            let task = self.transition(id, TaskState::Failed, Some(error.clone()))?;
            self.say(&task, ExpressionKind::Error, error).await;
            return Ok(());
        }
        // Reject the whole plan before running an invalid subset of tools.
        for step in &plan.steps {
            if self.registry.get(&step.capability).is_none()
                || !self.policy.is_granted(&self.id, &step.capability)
                || !step.args.is_object()
            {
                let error = format!("计划无效：{} 未授权、不存在或参数不是对象", step.capability);
                let task = self.transition(id, TaskState::Failed, Some(error.clone()))?;
                self.say(&task, ExpressionKind::Error, error).await;
                return Ok(());
            }
        }
        self.audit.append(AuditEvent::PlanCreated {
            intent: task.intent.id,
            agent: self.id.clone(),
            steps: plan
                .steps
                .iter()
                .map(|s| s.capability.to_string())
                .collect(),
        })?;
        let task = self.tasks.update(id, |t| {
            if !t.state.is_terminal() {
                t.steps = plan
                    .steps
                    .iter()
                    .map(|s| TaskStep {
                        capability: s.capability.clone(),
                        args: s.args.clone(),
                        state: StepState::Pending,
                        result: None,
                    })
                    .collect();
                t.tainted = plan.tainted;
                t.state = TaskState::Running;
            }
        })?;
        if task.state == TaskState::Cancelled {
            return Ok(());
        }
        self.log_state(&task)?;
        if task.steps.is_empty() {
            // A plan without steps is an answer only when the planner said
            // something and did not flag the intent as beyond it. "I cannot do
            // this" must never be recorded as work that succeeded.
            let state = if plan.unhandled || plan.reply.is_none() {
                TaskState::Unhandled
            } else {
                TaskState::Succeeded
            };
            let reply = plan.reply.unwrap_or_else(|| "我还不会做这件事".into());
            let task = self.transition(id, state, Some(reply.clone()))?;
            self.say(&task, ExpressionKind::Report, reply).await;
            return Ok(());
        }
        if let Some(reply) = plan.reply.as_ref() {
            self.say(&task, ExpressionKind::Plan, reply.clone()).await;
        }
        if !task.steps.is_empty() {
            let outline = task
                .steps
                .iter()
                .enumerate()
                .map(|(i, s)| format!("{}. {} {}", i + 1, s.capability, s.args))
                .collect::<Vec<_>>()
                .join("；");
            self.say(&task, ExpressionKind::Plan, outline).await;
        }
        self.run(id, None).await
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
    /// UID/source matching is still required; this is not L3 identity proof.
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
        self.audit.append(AuditEvent::HumanConfirmation {
            intent: task.intent.id,
            capability: task.steps[step].capability.clone(),
            approved: true,
        })?;
        let task = self.transition(id, TaskState::Running, None)?;
        if task.state != TaskState::Cancelled {
            self.run(id, Some(step)).await?;
        }
        Ok(())
    }

    async fn run(&mut self, id: TaskId, approved_step: Option<usize>) -> io::Result<()> {
        let Some(initial) = self.tasks.get(id) else {
            return Ok(());
        };
        for i in 0..initial.steps.len() {
            let task = self.tasks.get(id).expect("task exists");
            if task.state.is_terminal() {
                return Ok(());
            }
            if task.steps[i].state == StepState::Succeeded {
                continue;
            }
            let step = &task.steps[i];
            let Some(cap) = self.registry.get(&step.capability).cloned() else {
                self.transition(id, TaskState::Failed, Some("能力已卸载".into()))?;
                return Ok(());
            };
            let decision = self.policy.check(
                &self.id,
                cap.spec(),
                CallContext {
                    tainted: task.tainted,
                },
            );
            self.audit.append(AuditEvent::PolicyDecision {
                intent: task.intent.id,
                agent: self.id.clone(),
                capability: step.capability.clone(),
                decision: format!("{decision:?}"),
            })?;
            match decision {
                Decision::Deny { reason } => {
                    let task = self.transition(id, TaskState::Failed, Some(reason.clone()))?;
                    self.say(
                        &task,
                        ExpressionKind::Error,
                        format!("被策略拒绝：{reason}"),
                    )
                    .await;
                    return Ok(());
                }
                Decision::NeedsConfirmation {
                    verify_identity: true,
                    ..
                } => {
                    let msg = "此操作需要可信身份核验；当前输入设备不提供该能力，已拒绝执行";
                    let task = self.transition(id, TaskState::Failed, Some(msg.into()))?;
                    self.say(&task, ExpressionKind::Error, msg).await;
                    return Ok(());
                }
                Decision::NeedsConfirmation { reason, .. } if approved_step != Some(i) => {
                    let token = confirmation_token()?;
                    let task = self.tasks.update(id, |t| {
                        if !t.state.is_terminal() {
                            t.state = TaskState::AwaitingConfirmation;
                            t.message = Some(reason.clone());
                            t.confirmation = Some(Confirmation {
                                step: i,
                                expires_at_ms: now_ms().saturating_add(CONFIRMATION_TTL_MS),
                                token,
                            });
                        }
                    })?;
                    self.log_state(&task)?;
                    if task.state != TaskState::Cancelled {
                        self.say(&task, ExpressionKind::AskConfirm, format!(
                            "步骤 {}：{} {}。原因：{reason}。五分钟内输入 /approve {id} {} 批准，或 /cancel {id} 取消。可继续提交其他任务。",
                            i+1, step.capability, step.args, i+1,
                        )).await;
                    }
                    return Ok(());
                }
                _ => {}
            }
            // Both records must be durable before the capability gets control.
            self.audit.append(AuditEvent::CapabilityStarted {
                intent: task.intent.id,
                agent: self.id.clone(),
                capability: step.capability.clone(),
                args: step.args.clone(),
            })?;
            let started = self.tasks.update(id, |t| {
                if !t.state.is_terminal() {
                    t.state = TaskState::Running;
                    t.steps[i].state = StepState::Running;
                }
            })?;
            if started.state.is_terminal() {
                return Ok(());
            }
            let result = cap.invoke(step.args.clone()).await;
            let (ok, value) = match &result {
                Ok(v) => (true, v.clone()),
                Err(e) => (false, Value::String(e.to_string())),
            };
            self.audit.append(AuditEvent::CapabilityInvoked {
                intent: task.intent.id,
                agent: self.id.clone(),
                capability: step.capability.clone(),
                args: step.args.clone(),
                ok,
                result: value.clone(),
            })?;
            let task = self.tasks.update(id, |t| {
                t.steps[i].state = if ok {
                    StepState::Succeeded
                } else {
                    StepState::Failed
                };
                t.steps[i].result = Some(value.clone());
                if !ok && !t.state.is_terminal() {
                    t.state = TaskState::Failed;
                    t.message = Some(value.to_string());
                }
            })?;
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string());
            self.say(
                &task,
                if ok {
                    ExpressionKind::Report
                } else {
                    ExpressionKind::Error
                },
                message,
            )
            .await;
            if !ok {
                self.log_state(&task)?;
                return Ok(());
            }
        }
        self.transition(id, TaskState::Succeeded, Some("任务已完成".into()))?;
        Ok(())
    }

    async fn storage_error(&self, id: TaskId, error: io::Error) {
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

pub fn grant_manifest(policy: &mut PolicyEngine, agent: &AgentId, names: &[CapabilityName]) {
    for n in names {
        policy.grant(agent, n);
    }
}
