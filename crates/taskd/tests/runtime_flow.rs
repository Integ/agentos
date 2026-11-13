//! Durable execution and cancellation across the runtime/Butler boundary.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentos_audit::{AuditEvent, AuditLog};
use agentos_core::{
    now_ms, AgentId, Capability, CapabilityError, CapabilityName, CapabilitySpec, DeviceId, Intent,
    IntentId, RiskLevel, Urgency,
};
use agentos_planner::{Plan, Planner, RulePlanner, Step};
use agentos_policy::PolicyEngine;
use agentos_runtime::{StepState, TaskId, TaskState, TaskStore};
use agentos_services::expression::ExpressionHub;
use agentos_services::registry::CapabilityRegistry;
use agentos_taskd::builtin::{self, NoteStore};
use agentos_taskd::butler::{grant_manifest, Butler};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio::time::timeout;

static TEMP_ID: AtomicU64 = AtomicU64::new(0);
const WAIT_LIMIT: Duration = Duration::from_secs(5);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "agentos-runtime-flow-{label}-{}-{}-{}",
            std::process::id(),
            now_ms(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn intent(text: &str) -> Intent {
    Intent {
        // Input-device IDs may restart; the durable store assigns task identity.
        id: IntentId(1),
        source: DeviceId::new("keyboard"),
        speaker: Some("owner".into()),
        text: text.into(),
        urgency: Urgency::Normal,
        at_ms: now_ms(),
    }
}

fn butler(
    dir: &Path,
    registry: CapabilityRegistry,
    planner: Box<dyn Planner>,
    tasks: Arc<TaskStore>,
) -> Butler {
    let audit = Arc::new(AuditLog::open(dir.join("audit.jsonl")).unwrap());
    let id = AgentId::new("butler");
    let mut policy = PolicyEngine::new();
    let manifest = registry
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    grant_manifest(&mut policy, &id, &manifest);
    let expression = ExpressionHub::new(audit.clone());
    Butler::with_store(id, registry, policy, planner, audit, expression, tasks)
}

fn durable_notebook(dir: &Path) -> (Butler, NoteStore) {
    let tasks = Arc::new(TaskStore::open(dir.join("runtime")).unwrap());
    let mut registry = CapabilityRegistry::new();
    let notes = builtin::register_persistent(&mut registry, dir.join("notes.json")).unwrap();
    (butler(dir, registry, Box::new(RulePlanner), tasks), notes)
}

#[tokio::test]
async fn completed_tasks_and_notes_survive_restart_without_replaying() {
    let dir = TempDir::new("restart");
    let (first, notes) = durable_notebook(&dir.0);
    first.handle(intent("记一下 买牛奶")).await;
    assert_eq!(*notes.lock().unwrap(), vec!["买牛奶"]);
    let task = first.tasks().get(TaskId(1)).unwrap();
    assert_eq!(task.intent.id, IntentId(1));
    assert_eq!(task.state, TaskState::Succeeded);
    assert_eq!(task.steps[0].state, StepState::Succeeded);
    assert_eq!(task.steps[0].result.as_ref().unwrap()["count"], 1);
    drop(first);
    drop(notes);

    let (second, notes) = durable_notebook(&dir.0);
    assert_eq!(*notes.lock().unwrap(), vec!["买牛奶"]);
    assert_eq!(second.tasks().list().len(), 1);
    assert_eq!(
        second.tasks().get(TaskId(1)).unwrap().state,
        TaskState::Succeeded
    );
    // Even an explicit dispatch of an old task cannot repeat its side effect.
    second.execute(TaskId(1)).await;
    assert_eq!(*notes.lock().unwrap(), vec!["买牛奶"]);
    second.handle(intent("记一下 买面包")).await;
    assert_eq!(*notes.lock().unwrap(), vec!["买牛奶", "买面包"]);
    assert_eq!(
        second.tasks().get(TaskId(2)).unwrap().intent.id,
        IntentId(2)
    );
    assert_eq!(
        second.tasks().get(TaskId(2)).unwrap().state,
        TaskState::Succeeded
    );
    let entries = AuditLog::replay(dir.0.join("audit.jsonl")).unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|entry| matches!(&entry.event, AuditEvent::CapabilityInvoked { .. }))
            .count(),
        2
    );
    drop(second);
    drop(notes);

    let (third, notes) = durable_notebook(&dir.0);
    assert_eq!(*notes.lock().unwrap(), vec!["买牛奶", "买面包"]);
    assert_eq!(
        third
            .tasks()
            .list()
            .iter()
            .map(|task| task.id)
            .collect::<Vec<_>>(),
        vec![TaskId(1), TaskId(2)]
    );
    assert!(third
        .tasks()
        .list()
        .iter()
        .all(|task| task.state == TaskState::Succeeded));
}

fn capability_spec(name: &str) -> CapabilitySpec {
    CapabilitySpec {
        name: CapabilityName::new(name),
        description: name.into(),
        risk: RiskLevel::Reversible,
        input_schema: json!({"type":"object", "properties":{}, "additionalProperties":false}),
    }
}

fn step(name: &str) -> Step {
    Step::new(CapabilityName::new(name), json!({}))
}

#[derive(Clone)]
struct FixedPlanner(Plan);

#[async_trait]
impl Planner for FixedPlanner {
    async fn plan(&self, _intent: &Intent, _available: &[CapabilitySpec]) -> Plan {
        self.0.clone()
    }
}

struct GatedCapability {
    spec: CapabilitySpec,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Capability for GatedCapability {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, _args: Value) -> Result<Value, CapabilityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        Ok(json!({"message":"in-flight operation completed", "performed":true}))
    }
}

struct CountCapability {
    spec: CapabilitySpec,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Capability for CountCapability {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, _args: Value) -> Result<Value, CapabilityError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"message":"next step ran"}))
    }
}

#[tokio::test]
async fn cancelling_in_flight_capability_keeps_cancelled_and_skips_later_steps() {
    let dir = TempDir::new("cancel-capability");
    let tasks = Arc::new(TaskStore::open(dir.0.join("runtime")).unwrap());
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let gated_calls = Arc::new(AtomicUsize::new(0));
    let later_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = CapabilityRegistry::new();
    registry.register(Arc::new(GatedCapability {
        spec: capability_spec("test.gated"),
        entered: entered.clone(),
        release: release.clone(),
        calls: gated_calls.clone(),
    }));
    registry.register(Arc::new(CountCapability {
        spec: capability_spec("test.later"),
        calls: later_calls.clone(),
    }));
    let planner = FixedPlanner(Plan {
        steps: vec![step("test.gated"), step("test.later")],
        ..Plan::default()
    });
    let runner = butler(&dir.0, registry, Box::new(planner), tasks.clone());
    let worker = tokio::spawn(async move {
        runner.handle(intent("perform two steps")).await;
        runner
    });
    timeout(WAIT_LIMIT, entered.notified()).await.unwrap();
    let running = tasks.get(TaskId(1)).unwrap();
    assert_eq!(running.state, TaskState::Running);
    assert_eq!(running.steps[0].state, StepState::Running);
    assert_eq!(tasks.cancel(TaskId(1)).unwrap().state, TaskState::Cancelled);
    release.notify_one();
    let runner = timeout(WAIT_LIMIT, worker).await.unwrap().unwrap();
    let finished = tasks.get(TaskId(1)).unwrap();
    assert_eq!(finished.state, TaskState::Cancelled);
    assert_eq!(finished.steps[0].state, StepState::Succeeded);
    assert_eq!(
        finished.steps[0].result.as_ref().unwrap()["performed"],
        true
    );
    assert_eq!(finished.steps[1].state, StepState::Pending);
    assert_eq!(gated_calls.load(Ordering::SeqCst), 1);
    assert_eq!(later_calls.load(Ordering::SeqCst), 0);
    drop(runner);
    drop(tasks);
    let reopened = TaskStore::open(dir.0.join("runtime")).unwrap();
    assert_eq!(reopened.get(TaskId(1)).unwrap().state, TaskState::Cancelled);
    assert_eq!(later_calls.load(Ordering::SeqCst), 0);
}

struct GatedPlanner {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl Planner for GatedPlanner {
    async fn plan(&self, _intent: &Intent, _available: &[CapabilitySpec]) -> Plan {
        self.entered.notify_one();
        self.release.notified().await;
        Plan {
            steps: vec![step("test.later")],
            ..Plan::default()
        }
    }
}

#[tokio::test]
async fn cancelling_during_planning_prevents_capability_execution() {
    let dir = TempDir::new("cancel-planning");
    let tasks = Arc::new(TaskStore::open(dir.0.join("runtime")).unwrap());
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = CapabilityRegistry::new();
    registry.register(Arc::new(CountCapability {
        spec: capability_spec("test.later"),
        calls: calls.clone(),
    }));
    let planner = GatedPlanner {
        entered: entered.clone(),
        release: release.clone(),
    };
    let runner = butler(&dir.0, registry, Box::new(planner), tasks.clone());
    let worker = tokio::spawn(async move {
        runner.handle(intent("plan a side effect")).await;
        runner
    });
    timeout(WAIT_LIMIT, entered.notified()).await.unwrap();
    assert_eq!(tasks.get(TaskId(1)).unwrap().state, TaskState::Planning);
    tasks.cancel(TaskId(1)).unwrap();
    release.notify_one();
    let runner = timeout(WAIT_LIMIT, worker).await.unwrap().unwrap();
    let task = tasks.get(TaskId(1)).unwrap();
    assert_eq!(task.state, TaskState::Cancelled);
    assert!(task.steps.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let entries = AuditLog::replay(dir.0.join("audit.jsonl")).unwrap();
    assert!(!entries.iter().any(|entry| matches!(
        &entry.event,
        AuditEvent::CapabilityStarted { .. } | AuditEvent::CapabilityInvoked { .. }
    )));
    drop(runner);
    drop(tasks);
    assert_eq!(
        TaskStore::open(dir.0.join("runtime"))
            .unwrap()
            .get(TaskId(1))
            .unwrap()
            .state,
        TaskState::Cancelled
    );
}

#[tokio::test]
async fn cancelling_does_not_wait_for_the_planner_to_answer() {
    let dir = TempDir::new("cancel-model");
    let tasks = Arc::new(TaskStore::open(dir.0.join("runtime")).unwrap());
    let entered = Arc::new(Notify::new());
    let planner = GatedPlanner {
        entered: entered.clone(),
        // Never released: the planner would hang forever.
        release: Arc::new(Notify::new()),
    };
    let runner = butler(
        &dir.0,
        CapabilityRegistry::new(),
        Box::new(planner),
        tasks.clone(),
    );
    let worker = tokio::spawn(async move { runner.handle(intent("think forever")).await });
    timeout(WAIT_LIMIT, entered.notified()).await.unwrap();
    tasks.cancel(TaskId(1)).unwrap();
    // Guards in-flight cancellation: the worker is free again although the
    // model request never returned.
    timeout(WAIT_LIMIT, worker).await.unwrap().unwrap();
    let task = tasks.get(TaskId(1)).unwrap();
    assert_eq!(task.state, TaskState::Cancelled);
    assert!(task.steps.is_empty());
}

/// A call that hangs until it is dropped, and says so.
struct HangingCapability {
    spec: CapabilitySpec,
    abortable: bool,
    entered: Arc<Notify>,
    dropped: Arc<AtomicUsize>,
}

struct CountDrop(Arc<AtomicUsize>);

impl Drop for CountDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Capability for HangingCapability {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, _args: Value) -> Result<Value, CapabilityError> {
        let _dropped = CountDrop(self.dropped.clone());
        self.entered.notify_one();
        std::future::pending().await
    }

    fn abortable(&self) -> bool {
        self.abortable
    }
}

#[tokio::test]
async fn cancelling_abandons_an_abortable_call_and_records_its_outcome_as_unknown() {
    let dir = TempDir::new("cancel-abortable");
    let tasks = Arc::new(TaskStore::open(dir.0.join("runtime")).unwrap());
    let entered = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let later_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = CapabilityRegistry::new();
    registry.register(Arc::new(HangingCapability {
        spec: capability_spec("test.hanging"),
        abortable: true,
        entered: entered.clone(),
        dropped: dropped.clone(),
    }));
    registry.register(Arc::new(CountCapability {
        spec: capability_spec("test.later"),
        calls: later_calls.clone(),
    }));
    let planner = FixedPlanner(Plan {
        steps: vec![step("test.hanging"), step("test.later")],
        ..Plan::default()
    });
    let runner = butler(&dir.0, registry, Box::new(planner), tasks.clone());
    let worker = tokio::spawn(async move { runner.handle(intent("hang then continue")).await });
    timeout(WAIT_LIMIT, entered.notified()).await.unwrap();
    tasks.cancel(TaskId(1)).unwrap();
    timeout(WAIT_LIMIT, worker).await.unwrap().unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    let task = tasks.get(TaskId(1)).unwrap();
    assert_eq!(task.state, TaskState::Cancelled);
    // Guards honest outcomes: a call that was cut short is neither a success
    // nor "never ran", and a step with side effects says its effect is unknown.
    assert_eq!(task.steps[0].state, StepState::Failed);
    assert!(task.steps[0]
        .result
        .as_ref()
        .and_then(Value::as_str)
        .is_some_and(|text| text.contains("未知")));
    assert_eq!(task.steps[1].state, StepState::Pending);
    assert_eq!(later_calls.load(Ordering::SeqCst), 0);
    // Guards the audit trail: the start marker is closed by an interruption,
    // never by a result nobody saw.
    let entries = AuditLog::replay(dir.0.join("audit.jsonl")).unwrap();
    let count = |wanted: fn(&AuditEvent) -> bool| {
        entries.iter().filter(|entry| wanted(&entry.event)).count()
    };
    assert_eq!(
        count(|e| matches!(e, AuditEvent::CapabilityStarted { .. })),
        1
    );
    assert_eq!(
        count(|e| matches!(e, AuditEvent::CapabilityInterrupted { .. })),
        1
    );
    assert_eq!(
        count(|e| matches!(e, AuditEvent::CapabilityInvoked { .. })),
        0
    );
}

#[tokio::test]
async fn a_call_that_is_not_abortable_is_never_dropped_by_cancellation() {
    let dir = TempDir::new("cancel-not-abortable");
    let tasks = Arc::new(TaskStore::open(dir.0.join("runtime")).unwrap());
    let entered = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let mut registry = CapabilityRegistry::new();
    registry.register(Arc::new(HangingCapability {
        spec: capability_spec("test.hanging"),
        abortable: false,
        entered: entered.clone(),
        dropped: dropped.clone(),
    }));
    let planner = FixedPlanner(Plan {
        steps: vec![step("test.hanging")],
        ..Plan::default()
    });
    let runner = butler(&dir.0, registry, Box::new(planner), tasks.clone());
    let worker = tokio::spawn(async move { runner.handle(intent("hang")).await });
    timeout(WAIT_LIMIT, entered.notified()).await.unwrap();
    tasks.cancel(TaskId(1)).unwrap();
    // Guards side effects: the task is cancelled at once, but a call that
    // did not declare itself abortable keeps running to its own end.
    assert!(timeout(Duration::from_millis(200), worker).await.is_err());
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    let task = tasks.get(TaskId(1)).unwrap();
    assert_eq!(task.state, TaskState::Cancelled);
    assert_eq!(task.steps[0].state, StepState::Running);
}

#[tokio::test]
async fn a_read_only_call_is_abortable_unless_it_says_otherwise() {
    let spec = |risk| CapabilitySpec {
        risk,
        ..capability_spec("test.default")
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let capability = |risk| CountCapability {
        spec: spec(risk),
        calls: calls.clone(),
    };
    assert!(capability(RiskLevel::ReadOnly).abortable());
    for risk in [
        RiskLevel::Reversible,
        RiskLevel::External,
        RiskLevel::Irreversible,
    ] {
        assert!(!capability(risk).abortable());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_approvals_of_one_request_run_the_step_once() {
    let dir = TempDir::new("approve-race");
    let tasks = Arc::new(TaskStore::open(dir.0.join("runtime")).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = CapabilityRegistry::new();
    registry.register(Arc::new(CountCapability {
        spec: CapabilitySpec {
            risk: RiskLevel::External,
            ..capability_spec("test.send")
        },
        calls: calls.clone(),
    }));
    let planner = FixedPlanner(Plan {
        steps: vec![step("test.send")],
        ..Plan::default()
    });
    let runner = Arc::new(butler(&dir.0, registry, Box::new(planner), tasks.clone()));
    runner.handle(intent("send it")).await;
    let waiting = tasks.get(TaskId(1)).unwrap();
    assert_eq!(waiting.state, TaskState::AwaitingConfirmation);
    let token = waiting.confirmation.unwrap().token;
    let approvals: Vec<_> = (0..8)
        .map(|_| {
            let (runner, token) = (runner.clone(), token.clone());
            tokio::spawn(async move {
                runner
                    .approve_with_token(TaskId(1), 0, &intent("approve"), &token)
                    .await
            })
        })
        .collect();
    for approval in approvals {
        timeout(WAIT_LIMIT, approval).await.unwrap().unwrap();
    }
    // Guards "an approval is bound to one request": workers racing on the
    // same token carry it out exactly once.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(tasks.get(TaskId(1)).unwrap().state, TaskState::Succeeded);
}
