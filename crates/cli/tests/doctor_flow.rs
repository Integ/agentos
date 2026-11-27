//! `agentos doctor` against a real task service: it reports what the service
//! says about itself, what it can see when the service is down, and exits
//! with a code a script can act on.

#![cfg(unix)]

use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentos_audit::AuditLog;
use agentos_cli::client::Client;
use agentos_cli::doctor;
use agentos_cli::wait::wait_for_task;
use agentos_core::AgentId;
use agentos_ipc::status::{Severity, SystemStatus};
use agentos_ipc::{Command, Reply};
use agentos_planner::RulePlanner;
use agentos_policy::PolicyEngine;
use agentos_runtime::{TaskState, TaskStore};
use agentos_services::identity::PinVault;
use agentos_services::registry::CapabilityRegistry;
use agentos_services::schedule::ScheduleStore;
use agentos_taskd::builtin;
use agentos_taskd::butler::grant_manifest;
use agentos_taskd::service::Service;
use agentos_taskd::status::{Findings, StatusCapability};
use tokio::sync::oneshot;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Running {
    dir: PathBuf,
    socket: PathBuf,
    _stop: Option<oneshot::Sender<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn private_dir() -> PathBuf {
    // Short, private and owned by this user, as the service requires.
    let base = if cfg!(target_os = "macos") {
        "/private/tmp"
    } else {
        "/tmp"
    };
    let dir = PathBuf::from(format!(
        "{base}/aos-doc-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    dir
}

/// A service with the built-ins, and `system.status` when `with_status`.
async fn service(with_status: bool) -> Running {
    let dir = private_dir();
    let tasks = Arc::new(TaskStore::memory());
    let audit = Arc::new(AuditLog::open(dir.join("audit.jsonl")).unwrap());
    let mut registry = CapabilityRegistry::new();
    builtin::register_all(&mut registry);
    let socket = dir.join("control.sock");
    if with_status {
        let pins = Arc::new(PinVault::open(dir.join("pin.json")).unwrap());
        let schedules = Arc::new(ScheduleStore::open(&dir).unwrap());
        let status = Arc::new(StatusCapability::new(pins, schedules));
        registry.register(status.clone());
        status.publish(
            SystemStatus {
                version: env!("CARGO_PKG_VERSION").into(),
                state_dir: dir.display().to_string(),
                socket: socket.display().to_string(),
                planner: "离线规则规划器".into(),
                sandbox: "bubblewrap 沙箱".into(),
                ..SystemStatus::default()
            },
            &Findings {
                rule_planner: true,
                sandboxed: true,
                ..Findings::default()
            },
        );
    }
    let mut policy = PolicyEngine::new();
    let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
    grant_manifest(&mut policy, &AgentId::new("butler"), &names);
    let server = Service::new(tasks, audit, registry, policy, Box::new(RulePlanner))
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
        _stop: Some(stop),
    }
}

#[tokio::test]
async fn a_running_service_without_a_pin_is_reported_as_needing_attention() {
    let running = service(true).await;
    let report = doctor::examine(&Client::new(&running.socket), Some(&running.dir)).await;
    assert!(report.connected());
    assert_eq!(
        report.server_version.as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    let status = report.status.as_ref().expect("the service's own report");
    assert_eq!(status.planner, "离线规则规划器");
    // The PIN problem comes from the service; doctor lists it, most severe first.
    assert_eq!(report.problems[0].severity, Severity::Warning);
    assert!(report.problems[0].text.contains("PIN"));
    assert_eq!(report.problems.last().unwrap().severity, Severity::Info);
    // Contract: a warning makes the exit code 3, so a script can tell
    // "needs attention" from "down" (1) and "fine" (0).
    assert_eq!(report.exit_code(), 3);
    let text = report.text();
    assert!(text.contains("规划器：离线规则规划器") && text.contains("怎么办"));
    let json = report.json();
    assert_eq!(json["connected"], true);
    assert!(json["problems"].as_array().unwrap().len() >= 2);
    // Problems are listed once, at the top level.
    assert!(json["status"]["problems"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn asking_the_agent_for_its_status_reads_the_report_without_approval() {
    let running = service(true).await;
    let client = Client::new(&running.socket);
    let Reply::Accepted { task } = client
        .request(Command::Submit {
            text: "看看系统状态".into(),
            thread: None,
            spoken: false,
        })
        .await
        .unwrap()
    else {
        panic!("not accepted")
    };
    let outcome = wait_for_task(&client, task.id, Duration::from_secs(10))
        .await
        .unwrap();
    let task = outcome.task();
    assert_eq!(task.state, TaskState::Succeeded, "{task:?}");
    assert_eq!(task.steps[0].capability.as_str(), "system.status");
    let result = task.steps[0].result.as_ref().unwrap();
    assert!(result["message"].as_str().unwrap().contains("没有设置 PIN"));
    // Contract: the report is the service's own word (no outside program is
    // quoted in it), so reading it does not taint the task.
    assert!(!task.tainted);
}

#[tokio::test]
async fn an_older_service_still_gets_its_connection_checked() {
    let running = service(false).await;
    let report = doctor::examine(&Client::new(&running.socket), Some(&running.dir)).await;
    assert!(report.connected() && report.status.is_none());
    assert_eq!(report.problems.len(), 1);
    assert!(report.problems[0].text.contains("没有给出状态报告"));
    assert_eq!(report.exit_code(), 3);
}

#[tokio::test]
async fn a_stopped_service_is_named_with_how_to_start_it() {
    let dir = private_dir();
    let report = doctor::examine(&Client::new(dir.join("control.sock")), Some(&dir)).await;
    assert!(!report.connected());
    assert_eq!(report.exit_code(), 1);
    assert!(report.problems[0].text.contains("没有运行"));
    assert!(report.problems[0]
        .fix
        .as_deref()
        .unwrap()
        .contains("agentos-taskd"));
    let _ = std::fs::remove_dir_all(dir);
}
