//! What the workbench keeps in view against a real task service: a task
//! waiting for an approval, however many newer tasks there are, and the
//! activity feed that terminal-only users otherwise never see.

#![cfg(unix)]

use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentos_audit::AuditLog;
use agentos_cli::client::Client;
use agentos_cli::render;
use agentos_cli::workbench::waiting_on_user;
use agentos_core::{
    AgentId, Capability, CapabilityError, CapabilityName, CapabilitySpec, Intent, RiskLevel,
};
use agentos_ipc::{Command, Reply};
use agentos_planner::{Plan, Planner, Step};
use agentos_policy::PolicyEngine;
use agentos_runtime::{Task, TaskId, TaskState, TaskStore};
use agentos_services::registry::CapabilityRegistry;
use agentos_taskd::builtin;
use agentos_taskd::butler::grant_manifest;
use agentos_taskd::service::Service;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::oneshot;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// An L2 capability: every call needs the user's approval.
struct SendMail;

#[async_trait]
impl Capability for SendMail {
    fn spec(&self) -> &CapabilitySpec {
        static SPEC: std::sync::OnceLock<CapabilitySpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| CapabilitySpec {
            name: CapabilityName::new("test.send"),
            description: "test only".into(),
            risk: RiskLevel::External,
            input_schema: json!({"type": "object"}),
        })
    }

    async fn invoke(&self, _args: Value) -> Result<Value, CapabilityError> {
        Ok(json!({"message": "sent"}))
    }
}

/// "发给 alice" sends mail (and waits for approval); anything else asks the
/// time, which runs at once.
struct ScriptedPlanner;

#[async_trait]
impl Planner for ScriptedPlanner {
    async fn plan(&self, intent: &Intent, _available: &[CapabilitySpec]) -> Plan {
        let step = if intent.text == "发给 alice" {
            Step::new(CapabilityName::new("test.send"), json!({"to": "alice"}))
        } else {
            Step::new(CapabilityName::new("clock.now"), json!({}))
        };
        Plan {
            steps: vec![step],
            ..Plan::default()
        }
    }
}

struct Running {
    dir: PathBuf,
    socket: PathBuf,
    tasks: Arc<TaskStore>,
    _stop: oneshot::Sender<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn service() -> Running {
    // Short, private and owned by this user, as the service requires.
    let base = if cfg!(target_os = "macos") {
        "/private/tmp"
    } else {
        "/tmp"
    };
    let dir = PathBuf::from(format!(
        "{base}/aos-wb-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let tasks = Arc::new(TaskStore::memory());
    let audit = Arc::new(AuditLog::open(dir.join("audit.jsonl")).unwrap());
    let mut registry = CapabilityRegistry::new();
    builtin::register_all(&mut registry);
    registry.register(Arc::new(SendMail));
    let mut policy = PolicyEngine::new();
    let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
    grant_manifest(&mut policy, &AgentId::new("butler"), &names);
    let socket = dir.join("control.sock");
    let server = Service::new(
        tasks.clone(),
        audit,
        registry,
        policy,
        Box::new(ScriptedPlanner),
    )
    .bind(&socket)
    .await
    .unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(server.run_until(async {
        let _ = stopped.await;
    }));
    Running {
        dir,
        socket,
        tasks,
        _stop: stop,
    }
}

async fn submit(client: &Client, text: &str) -> TaskId {
    match client
        .request(Command::Submit {
            text: text.into(),
            thread: None,
            spoken: false,
        })
        .await
        .unwrap()
    {
        Reply::Accepted { task } => task.id,
        other => panic!("not accepted: {other:?}"),
    }
}

async fn settled(tasks: &TaskStore, id: TaskId, state: TaskState) -> Task {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(task) = tasks.get(id).filter(|t| t.state == state) {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{id} never reached {state:?}"))
}

#[tokio::test]
async fn a_task_waiting_for_approval_stays_in_view_after_newer_tasks_fill_the_page() {
    let running = service().await;
    let client = Client::new(&running.socket);
    let mail = submit(&client, "发给 alice").await;
    let pending = settled(&running.tasks, mail, TaskState::AwaitingConfirmation).await;
    // More newer tasks than the workbench's page holds.
    for _ in 0..13 {
        let id = submit(&client, "现在几点").await;
        settled(&running.tasks, id, TaskState::Succeeded).await;
    }
    let Reply::Tasks { tasks: page, .. } = client
        .request(Command::List {
            before: None,
            limit: 12,
            state: None,
            thread: None,
        })
        .await
        .unwrap()
    else {
        panic!("not a page")
    };
    assert!(
        page.iter().all(|task| task.id != mail),
        "off the first page"
    );

    // Contract: what waits on the user is found on every page.
    let waiting = waiting_on_user(&client).await.unwrap();
    assert_eq!(
        waiting.tasks.iter().map(|t| t.id).collect::<Vec<_>>(),
        [mail]
    );
    assert!(!waiting.more);
    // This service has no `system.status`: the tasks still count.
    assert_eq!(waiting.problems, None);
    assert!(render::waiting_strip(&waiting)
        .unwrap()
        .contains(&mail.to_string()));

    // Approved, it no longer waits, and the feed says who decided what.
    let confirmation = pending.confirmation.unwrap();
    client
        .request(Command::Approve {
            id: mail,
            step: confirmation.step + 1,
            confirmation: confirmation.token,
            pin: None,
        })
        .await
        .unwrap();
    settled(&running.tasks, mail, TaskState::Succeeded).await;
    let waiting = waiting_on_user(&client).await.unwrap();
    assert!(waiting.tasks.is_empty());
    assert_eq!(render::waiting_strip(&waiting), None);

    let activity = client
        .request(Command::Activity { limit: 50 })
        .await
        .unwrap();
    let text = render::reply_text(&activity);
    assert!(text.contains("你批准了：test.send"), "{text}");
    assert!(
        text.contains("clock.now") && text.contains("test.send"),
        "{text}"
    );
    // Oldest first: the approval comes after the first clock reading.
    assert!(text.find("clock.now").unwrap() < text.find("你批准了").unwrap());
}
