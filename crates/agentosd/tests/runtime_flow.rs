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
use agentos_policy::PolicyEngine;
use agentos_runtime::{StepState, TaskId, TaskState, TaskStore};
use agentosd::builtin::{self, NoteStore};
use agentosd::butler::{grant_manifest, Butler};
use agentosd::expression::ExpressionHub;
use agentosd::planner::{Plan, Planner, RulePlanner, Step};
use agentosd::registry::CapabilityRegistry;
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
    let (mut first, notes) = durable_notebook(&dir.0);
    first.handle(intent("记一下 买牛奶")).await;
    assert_eq!(*notes.lock().unwrap(), vec!["买牛奶"]);
    let task = first.tasks().get(TaskId(1)).unwrap();
    assert_eq!(task.intent.id, IntentId(1));
    assert_eq!(task.state, TaskState::Succeeded);
    assert_eq!(task.steps[0].state, StepState::Succeeded);
    assert_eq!(task.steps[0].result.as_ref().unwrap()["count"], 1);
    drop(first);
    drop(notes);

    let (mut second, notes) = durable_notebook(&dir.0);
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
    Step {
        capability: CapabilityName::new(name),
        args: json!({}),
    }
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
    let mut runner = butler(&dir.0, registry, Box::new(planner), tasks.clone());
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
    let mut runner = butler(&dir.0, registry, Box::new(planner), tasks.clone());
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
