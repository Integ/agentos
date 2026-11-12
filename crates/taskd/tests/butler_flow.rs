//! End-to-end: intents in, expressions and audit entries out.

use std::sync::{Arc, Mutex};

use agentos_audit::{AuditEntry, AuditEvent, AuditLog};
use agentos_core::{
    now_ms, AgentId, Capability, CapabilityError, CapabilityName, CapabilitySpec, DeviceId,
    DeviceInfo, Expression, ExpressionKind, Intent, IntentId, Modality, Output, OutputError,
    PrivacyClass, RiskLevel, Urgency,
};
use agentos_planner::{Plan, Planner, RulePlanner, Step};
use agentos_policy::PolicyEngine;
use agentos_runtime::{StepState, Task, TaskId, TaskState};
use agentos_services::expression::ExpressionHub;
use agentos_services::registry::CapabilityRegistry;
use agentos_taskd::builtin::{self, NoteStore};
use agentos_taskd::butler::{grant_manifest, Butler};
use async_trait::async_trait;
use serde_json::{json, Value};

struct TestCapability {
    spec: CapabilitySpec,
    calls: Mutex<Vec<Value>>,
    fail: bool,
}

impl TestCapability {
    fn new(name: &str, risk: RiskLevel, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            spec: CapabilitySpec {
                name: CapabilityName::new(name),
                description: "test capability".into(),
                risk,
                input_schema: json!({"type": "object"}),
            },
            calls: Mutex::default(),
            fail,
        })
    }

    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl Capability for TestCapability {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        self.calls.lock().unwrap().push(args);
        if self.fail {
            Err(CapabilityError::Failed("test failure".into()))
        } else {
            Ok(json!({"message": "test success"}))
        }
    }
}

/// Keep ordinary intents useful while a deterministic test plan is waiting.
struct TestPlanner(Plan);

#[async_trait]
impl Planner for TestPlanner {
    async fn plan(&self, intent: &Intent, available: &[CapabilitySpec]) -> Plan {
        if intent.text == "测试计划" {
            self.0.clone()
        } else {
            RulePlanner.plan(intent, available).await
        }
    }
}

fn plan(names: &[&str]) -> Plan {
    Plan {
        steps: names
            .iter()
            .map(|name| Step::new(CapabilityName::new(*name), json!({})))
            .collect(),
        ..Plan::default()
    }
}

fn intent(text: &str, source: &str, speaker: Option<&str>) -> Intent {
    Intent {
        id: IntentId(999),
        source: DeviceId::new(source),
        speaker: speaker.map(str::to_string),
        text: text.into(),
        urgency: Urgency::Normal,
        at_ms: now_ms(),
    }
}

fn responder() -> Intent {
    intent("/approve task-1 1", "test", Some("adele"))
}

#[derive(Default)]
struct Recorder(Mutex<Vec<Expression>>);

#[async_trait]
impl Output for Recorder {
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::new("recorder"),
            kind: "test".into(),
            location: None,
            privacy: PrivacyClass::Public,
        }
    }
    fn modalities(&self) -> &[Modality] {
        &[Modality::Visual]
    }
    async fn express(&self, e: &Expression) -> Result<(), OutputError> {
        self.0.lock().unwrap().push(e.clone());
        Ok(())
    }
}

impl Recorder {
    fn last(&self) -> Expression {
        self.0
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("no expression")
    }
}

struct Harness {
    butler: Butler,
    out: Arc<Recorder>,
    notes: NoteStore,
    audit_path: std::path::PathBuf,
    next: u64,
}

impl Harness {
    fn new(name: &str, grant: impl Fn(&CapabilityName) -> bool) -> Self {
        Self::configured(name, grant, Box::new(RulePlanner), vec![])
    }

    fn with_plan(name: &str, plan: Plan, capabilities: Vec<Arc<dyn Capability>>) -> Self {
        Self::configured(name, |_| true, Box::new(TestPlanner(plan)), capabilities)
    }

    fn configured(
        name: &str,
        grant: impl Fn(&CapabilityName) -> bool,
        planner: Box<dyn Planner>,
        capabilities: Vec<Arc<dyn Capability>>,
    ) -> Self {
        let audit_path = std::env::temp_dir().join(format!(
            "agentosd-{name}-{}-{}.jsonl",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_file(&audit_path);
        let audit = Arc::new(AuditLog::open(&audit_path).unwrap());
        let mut registry = CapabilityRegistry::new();
        let notes = builtin::register_all(&mut registry);
        for capability in capabilities {
            registry.register(capability);
        }
        let id = AgentId::new("butler");
        let mut policy = PolicyEngine::new();
        let manifest: Vec<_> = registry
            .specs()
            .into_iter()
            .map(|s| s.name)
            .filter(|n| grant(n))
            .collect();
        grant_manifest(&mut policy, &id, &manifest);
        let out = Arc::new(Recorder::default());
        let mut expr = ExpressionHub::new(audit.clone());
        expr.add_output(out.clone());
        let butler = Butler::new(id, registry, policy, planner, audit, expr);
        Self {
            butler,
            out,
            notes,
            audit_path,
            next: 0,
        }
    }

    async fn say(&mut self, text: &str) -> Expression {
        self.next += 1;
        self.butler
            .handle(Intent {
                id: IntentId(self.next),
                source: DeviceId::new("test"),
                speaker: Some("adele".into()),
                text: text.into(),
                urgency: Urgency::Normal,
                at_ms: now_ms(),
            })
            .await;
        self.out.last()
    }

    fn notes(&self) -> Vec<String> {
        self.notes.lock().unwrap().clone()
    }

    fn task(&self, id: u64) -> Task {
        self.butler.tasks().get(TaskId(id)).expect("task missing")
    }

    fn entries(&self) -> Vec<AuditEntry> {
        AuditLog::replay(&self.audit_path).unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.audit_path);
    }
}

#[tokio::test]
async fn clearing_notes_is_reversible_and_tracks_completed_tasks() {
    let mut h = Harness::new("notebook", |_| true);

    let r = h.say("记一下 买牛奶").await;
    assert_eq!(r.kind, ExpressionKind::Report);
    assert_eq!(h.notes(), vec!["买牛奶"]);
    assert_eq!(h.task(1).state, TaskState::Succeeded);

    let r = h.say("清空笔记").await;
    assert_eq!(r.kind, ExpressionKind::Report);
    assert!(!h.butler.awaiting_confirmation());
    assert!(h.notes().is_empty());
    assert_eq!(h.task(2).state, TaskState::Succeeded);

    h.say("恢复笔记").await;
    assert_eq!(h.notes(), vec!["买牛奶"]);
    assert_eq!(h.task(3).state, TaskState::Succeeded);

    let entries = h.entries();
    assert!(!entries
        .iter()
        .any(|entry| matches!(entry.event, AuditEvent::HumanConfirmation { .. })));
    assert!(entries.windows(2).all(|w| w[1].seq == w[0].seq + 1));
}

#[tokio::test]
async fn approval_is_bound_to_task_step_source_and_speaker_and_is_single_use() {
    let external = TestCapability::new("test.external", RiskLevel::External, false);
    let mut h = Harness::with_plan(
        "bound-approval",
        plan(&["test.external"]),
        vec![external.clone()],
    );
    assert_eq!(h.say("测试计划").await.kind, ExpressionKind::AskConfirm);
    assert_eq!(h.task(1).state, TaskState::AwaitingConfirmation);
    assert_eq!(external.count(), 0);

    h.butler.approve(TaskId(999), 0, &responder()).await;
    for (step, reply) in [
        (0, intent("approve", "another-device", Some("adele"))),
        (0, intent("approve", "test", Some("someone-else"))),
        (0, intent("approve", "test", None)),
        (1, responder()),
    ] {
        h.butler.approve(TaskId(1), step, &reply).await;
        assert_eq!(h.task(1).state, TaskState::AwaitingConfirmation);
        assert_eq!(external.count(), 0);
        assert_eq!(h.out.last().kind, ExpressionKind::Error);
    }

    h.butler.approve(TaskId(1), 0, &responder()).await;
    assert_eq!(h.task(1).state, TaskState::Succeeded);
    assert!(h.task(1).confirmation.is_none());
    assert_eq!(external.count(), 1);
    h.butler.approve(TaskId(1), 0, &responder()).await;
    assert_eq!(h.task(1).state, TaskState::Succeeded);
    assert_eq!(external.count(), 1);
    assert_eq!(
        h.entries()
            .iter()
            .filter(|entry| matches!(
                entry.event,
                AuditEvent::HumanConfirmation { approved: true, .. }
            ))
            .count(),
        1
    );
}

#[tokio::test]
async fn expired_approval_cannot_invoke_a_capability() {
    let external = TestCapability::new("test.external", RiskLevel::External, false);
    let mut h = Harness::with_plan("expired", plan(&["test.external"]), vec![external.clone()]);
    h.say("测试计划").await;
    let expired_at = h.task(1).created_at_ms + 1;
    while now_ms() <= expired_at {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    h.butler
        .tasks()
        .update(TaskId(1), |task| {
            task.confirmation.as_mut().unwrap().expires_at_ms = expired_at;
        })
        .unwrap();
    h.butler.approve(TaskId(1), 0, &responder()).await;
    assert_eq!(external.count(), 0);
    assert_eq!(h.out.last().kind, ExpressionKind::Error);
    assert!(!h.entries().iter().any(|entry| matches!(
        entry.event,
        AuditEvent::HumanConfirmation { approved: true, .. }
    )));
}

#[tokio::test]
async fn confirmation_tokens_are_unique_and_cannot_approve_another_prompt() {
    let external = TestCapability::new("test.external", RiskLevel::External, false);
    let mut h = Harness::with_plan("tokens", plan(&["test.external"]), vec![external.clone()]);
    h.say("测试计划").await;
    let first = h.task(1).confirmation.unwrap().token;
    assert!(h.task(1).message.is_some_and(|message| !message.is_empty()));
    h.butler.tasks().cancel(TaskId(1)).unwrap();
    h.say("测试计划").await;
    let second = h.task(2).confirmation.unwrap().token;
    assert_eq!(first.len(), 64);
    assert_ne!(first, second);
    h.butler
        .approve_with_token(TaskId(2), 0, &responder(), &first)
        .await;
    assert_eq!(external.count(), 0);
    assert_eq!(h.task(2).state, TaskState::AwaitingConfirmation);
    h.butler
        .approve_with_token(TaskId(2), 0, &responder(), &second)
        .await;
    assert_eq!(external.count(), 1);
    assert_eq!(h.task(2).state, TaskState::Succeeded);
    h.butler
        .approve_with_token(TaskId(1), 0, &responder(), &first)
        .await;
    assert_eq!(external.count(), 1);
}

#[tokio::test]
async fn awaiting_confirmation_does_not_consume_new_intents_or_implicit_yes() {
    let external = TestCapability::new("test.external", RiskLevel::External, false);
    let mut h = Harness::with_plan(
        "independent",
        plan(&["test.external"]),
        vec![external.clone()],
    );
    h.say("测试计划").await;
    h.say("记一下 买牛奶").await;
    h.say("y").await;
    assert_eq!(h.notes(), vec!["买牛奶"]);
    assert_eq!(h.butler.tasks().list().len(), 3);
    assert_eq!(h.task(1).state, TaskState::AwaitingConfirmation);
    assert_eq!(h.task(2).state, TaskState::Succeeded);
    // A bare "y" is a new intent nobody can act on, not an approval and not
    // completed work.
    assert_eq!(h.task(3).state, TaskState::Unhandled);
    assert_eq!(external.count(), 0);
}

#[tokio::test]
async fn approval_rechecks_a_revoked_grant() {
    let external = TestCapability::new("test.external", RiskLevel::External, false);
    let mut h = Harness::with_plan("revoked", plan(&["test.external"]), vec![external.clone()]);
    h.say("测试计划").await;
    let id = h.butler.id().clone();
    h.butler
        .policy_mut()
        .revoke(&id, &CapabilityName::new("test.external"));
    h.butler.approve(TaskId(1), 0, &responder()).await;
    assert_eq!(h.task(1).state, TaskState::Failed);
    assert_eq!(external.count(), 0);
    assert_eq!(h.out.last().kind, ExpressionKind::Error);
}

#[tokio::test]
async fn irreversible_actions_fail_closed_without_an_identity_verifier() {
    let irreversible = TestCapability::new("test.irreversible", RiskLevel::Irreversible, false);
    let mut h = Harness::with_plan(
        "identity-required",
        plan(&["test.irreversible"]),
        vec![irreversible.clone()],
    );
    h.say("测试计划").await;
    assert_eq!(h.task(1).state, TaskState::Failed);
    assert!(!h.butler.awaiting_confirmation());
    assert_eq!(h.out.last().kind, ExpressionKind::Error);
    h.butler.approve(TaskId(1), 0, &responder()).await;
    assert_eq!(irreversible.count(), 0);
    assert!(!h.entries().iter().any(|entry| matches!(
        entry.event,
        AuditEvent::CapabilityStarted { .. } | AuditEvent::CapabilityInvoked { .. }
    )));
}

#[tokio::test]
async fn an_unknown_later_tool_rejects_the_entire_plan_before_any_side_effect() {
    let first = TestCapability::new("test.first", RiskLevel::Reversible, false);
    let mut h = Harness::with_plan(
        "preflight",
        plan(&["test.first", "unknown.tool"]),
        vec![first.clone()],
    );
    h.say("测试计划").await;
    assert_eq!(h.task(1).state, TaskState::Failed);
    assert_eq!(first.count(), 0);
    assert!(!h
        .entries()
        .iter()
        .any(|entry| matches!(entry.event, AuditEvent::CapabilityStarted { .. })));
}

#[tokio::test]
async fn capability_failure_stops_the_remaining_plan() {
    let first = TestCapability::new("test.first", RiskLevel::Reversible, true);
    let second = TestCapability::new("test.second", RiskLevel::Reversible, false);
    let mut h = Harness::with_plan(
        "failed-step",
        plan(&["test.first", "test.second"]),
        vec![first.clone(), second.clone()],
    );
    h.say("测试计划").await;
    assert_eq!(h.task(1).state, TaskState::Failed);
    assert_eq!(h.task(1).steps[0].state, StepState::Failed);
    // Never started, and recorded as such rather than left looking pending.
    assert_eq!(h.task(1).steps[1].state, StepState::Skipped);
    assert_eq!(first.count(), 1);
    assert_eq!(second.count(), 0);
    assert_eq!(h.out.last().kind, ExpressionKind::Error);
}

#[tokio::test]
async fn every_invocation_has_an_earlier_write_ahead_audit_record() {
    let first = TestCapability::new("test.first", RiskLevel::Reversible, false);
    let second = TestCapability::new("test.second", RiskLevel::Reversible, false);
    let mut h = Harness::with_plan(
        "write-ahead",
        plan(&["test.first", "test.second"]),
        vec![first.clone(), second.clone()],
    );
    h.say("测试计划").await;
    assert_eq!(h.task(1).state, TaskState::Succeeded);
    assert_eq!((first.count(), second.count()), (1, 1));
    let events = h
        .entries()
        .into_iter()
        .filter_map(|entry| match entry.event {
            AuditEvent::CapabilityStarted { capability, .. } => Some(("started", capability)),
            AuditEvent::CapabilityInvoked { capability, ok, .. } => {
                assert!(ok);
                Some(("invoked", capability))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        events,
        vec![
            ("started", CapabilityName::new("test.first")),
            ("invoked", CapabilityName::new("test.first")),
            ("started", CapabilityName::new("test.second")),
            ("invoked", CapabilityName::new("test.second")),
        ]
    );
}

#[tokio::test]
async fn a_failed_plan_never_executes_even_if_it_contains_steps() {
    let capability = TestCapability::new("test.first", RiskLevel::Reversible, false);
    let mut failed = plan(&["test.first"]);
    failed.error = Some("incomplete plan".into());
    let mut h = Harness::with_plan("planner-error", failed, vec![capability.clone()]);
    h.say("测试计划").await;
    assert_eq!(h.task(1).state, TaskState::Failed);
    assert_eq!(capability.count(), 0);
    assert_eq!(h.out.last().kind, ExpressionKind::Error);
}

#[cfg(unix)]
#[tokio::test]
async fn unavailable_audit_storage_prevents_capability_execution() {
    // /dev/null reads as an empty log, but cannot make writes durable with
    // sync_data. This exercises a real I/O failure through the public API.
    let audit = Arc::new(AuditLog::open("/dev/null").unwrap());
    let mut registry = CapabilityRegistry::new();
    let capability = TestCapability::new("test.first", RiskLevel::Reversible, false);
    registry.register(capability.clone());
    let id = AgentId::new("butler");
    let mut policy = PolicyEngine::new();
    policy.grant(&id, &capability.spec.name);
    let recorder = Arc::new(Recorder::default());
    let mut expr = ExpressionHub::new(audit.clone());
    expr.add_output(recorder.clone());
    let mut butler = Butler::new(
        id,
        registry,
        policy,
        Box::new(TestPlanner(plan(&["test.first"]))),
        audit,
        expr,
    );
    butler
        .handle(intent("测试计划", "test", Some("adele")))
        .await;
    assert_eq!(capability.count(), 0);
    assert_eq!(
        butler.tasks().get(TaskId(1)).unwrap().state,
        TaskState::Failed
    );
    assert_eq!(recorder.last().kind, ExpressionKind::Error);
}

#[tokio::test]
async fn ungranted_capability_is_denied() {
    let mut h = Harness::new("deny", |n| n.as_str() != "notes.append");
    let r = h.say("记一下 买牛奶").await;
    assert_eq!(r.kind, ExpressionKind::Error);
    assert!(h.notes().is_empty());
}

#[tokio::test]
async fn unknown_request_lists_capabilities() {
    let mut h = Harness::new("unknown", |_| true);
    let r = h.say("讲个笑话").await;
    assert_eq!(r.kind, ExpressionKind::Report);
    assert!(r.text.contains("notes.list"));
    // Explaining what is possible is not the same as having done something.
    let task = h.task(1);
    assert_eq!(task.state, TaskState::Unhandled);
    assert!(task.steps.is_empty());
    assert!(task.message.unwrap().contains("notes.list"));
}

#[tokio::test]
async fn a_direct_answer_succeeds_but_silence_or_inability_does_not() {
    for (label, plan, expected) in [
        (
            "answer",
            Plan {
                reply: Some("二加二等于四。".into()),
                ..Plan::default()
            },
            TaskState::Succeeded,
        ),
        (
            "inability",
            Plan::unhandled("我做不到这件事。"),
            TaskState::Unhandled,
        ),
        ("silence", Plan::default(), TaskState::Unhandled),
        ("failure", Plan::failed("无法连接模型"), TaskState::Failed),
    ] {
        let mut h = Harness::with_plan(label, plan, vec![]);
        h.say("测试计划").await;
        let task = h.task(1);
        assert_eq!(task.state, expected, "{label}");
        assert!(task.steps.is_empty() && task.message.is_some(), "{label}");
    }
}
