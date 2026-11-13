//! The MCP bridge against a real task service: an outside agent gets the
//! tools of one registered caller and nothing of the user's.

#![cfg(unix)]

use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use agentos_audit::AuditLog;
use agentos_cli::client::Client;
use agentos_cli::mcp::respond;
use agentos_core::{AgentId, RiskLevel};
use agentos_ipc::{Command, Reply};
use agentos_planner::RulePlanner;
use agentos_policy::PolicyEngine;
use agentos_runtime::{TaskState, TaskStore};
use agentos_services::registry::CapabilityRegistry;
use agentos_taskd::builtin;
use agentos_taskd::butler::grant_manifest;
use agentos_taskd::service::Service;
use serde_json::{json, Value};
use tokio::sync::oneshot;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
        "{base}/aos-mcp-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let tasks = Arc::new(TaskStore::memory());
    let audit = Arc::new(AuditLog::open(dir.join("audit.jsonl")).unwrap());
    let mut registry = CapabilityRegistry::new();
    builtin::register_all(&mut registry);
    let mut policy = PolicyEngine::new();
    let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
    grant_manifest(&mut policy, &AgentId::new("butler"), &names);
    let socket = dir.join("control.sock");
    let server = Service::new(
        tasks.clone(),
        audit,
        registry,
        policy,
        Box::new(RulePlanner),
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

/// The text of a tool result and whether it is an error.
fn told(response: &Value) -> (String, bool) {
    let result = &response["result"];
    (
        result["content"][0]["text"].as_str().unwrap().to_string(),
        result["isError"].as_bool().unwrap(),
    )
}

async fn call(client: &Client, id: u64, name: &str, arguments: Value) -> (String, bool) {
    let response = respond(
        client,
        &json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}}),
    )
    .await
    .unwrap();
    assert_eq!(response["id"], id);
    told(&response)
}

#[tokio::test]
async fn an_outside_agent_gets_one_callers_tools_and_no_way_to_approve() {
    let running = service().await;
    let user = Client::new(&running.socket);
    user.request(Command::RegisterCaller {
        id: "helper".into(),
        name: "测试助手".into(),
        capabilities: vec!["notes.list".into(), "notes.append".into()],
        max_risk: RiskLevel::Reversible,
        pin: None,
    })
    .await
    .unwrap();
    let Reply::Accepted { task: private } = user
        .request(Command::Submit {
            text: "记一下 我的秘密".into(),
            thread: None,
        })
        .await
        .unwrap()
    else {
        panic!("not accepted")
    };
    let bridge = Client::new(&running.socket).as_caller("helper");

    // The handshake: a revision the client asked for if it is known, the
    // newest otherwise; notifications get no answer.
    let hello = respond(
        &bridge,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}}}),
    )
    .await
    .unwrap();
    assert_eq!(hello["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(hello["result"]["serverInfo"]["name"], "agentos");
    let newest = respond(
        &bridge,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "initialize",
            "params": {"protocolVersion": "1999-01-01"}}),
    )
    .await
    .unwrap();
    assert_eq!(newest["result"]["protocolVersion"], "2025-06-18");
    assert!(respond(
        &bridge,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .await
    .is_none());
    let unknown = respond(
        &bridge,
        &json!({"jsonrpc": "2.0", "id": 3, "method": "nope"}),
    )
    .await
    .unwrap();
    assert_eq!(unknown["error"]["code"], -32601);

    // Guards "a caller cannot approve": there is no such tool to call.
    let listed = respond(
        &bridge,
        &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"}),
    )
    .await
    .unwrap();
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "submit_intent",
            "get_task",
            "list_tasks",
            "cancel_task",
            "reply_to_task",
            "list_capabilities",
            "read_capability"
        ]
    );

    // What it may use is its manifest; reading outside it is nothing.
    let (capabilities, error) = call(&bridge, 5, "list_capabilities", json!({})).await;
    assert!(!error);
    assert!(capabilities.contains("notes.list") && !capabilities.contains("clock.now"));
    let (_, error) = call(
        &bridge,
        6,
        "read_capability",
        json!({"capability": "clock.now"}),
    )
    .await;
    assert!(error);
    let (notes, error) = call(
        &bridge,
        7,
        "read_capability",
        json!({"capability": "notes.list"}),
    )
    .await;
    assert!(!error, "{notes}");

    // A read finishes on its own.
    let (done, error) = call(&bridge, 8, "submit_intent", json!({"text": "看看笔记"})).await;
    assert!(!error, "{done}");
    let done: Value = serde_json::from_str(&done).unwrap();
    assert_eq!(done["state"], "succeeded");

    // A write waits for the human: the agent is told so, is not given the
    // token, and nothing was written.
    let (waiting, error) = call(
        &bridge,
        9,
        "submit_intent",
        json!({"text": "记一下 来自外部", "wait_seconds": 5}),
    )
    .await;
    assert!(!error, "{waiting}");
    let view: Value = serde_json::from_str(&waiting).unwrap();
    assert_eq!(view["state"], "awaiting_confirmation");
    assert!(view["note"].as_str().unwrap().contains("用户本人"));
    assert!(!waiting.contains("token"), "{waiting}");
    assert!(view.get("confirmation").is_none());
    let id = view["id"].as_u64().unwrap();
    let token = running
        .tasks
        .get(agentos_runtime::TaskId(id))
        .unwrap()
        .confirmation
        .unwrap()
        .token;
    assert!(!waiting.contains(&token));

    // The user's tasks do not exist for it, and its list is its own.
    let (_, error) = call(&bridge, 10, "get_task", json!({"id": private.id})).await;
    assert!(error);
    let (listing, _) = call(&bridge, 11, "list_tasks", json!({})).await;
    assert!(!listing.contains("我的秘密"), "{listing}");
    let listing: Value = serde_json::from_str(&listing).unwrap();
    assert_eq!(listing.as_array().unwrap().len(), 2);

    // It may give up on its own request.
    let (cancelled, error) = call(&bridge, 12, "cancel_task", json!({"id": id})).await;
    assert!(!error, "{cancelled}");
    assert_eq!(
        running
            .tasks
            .get(agentos_runtime::TaskId(id))
            .unwrap()
            .state,
        TaskState::Cancelled
    );

    // Mistakes are answered, not guessed at.
    for (name, arguments) in [
        ("approve_task", json!({"id": id})),
        ("get_task", json!({"id": id, "extra": true})),
        ("get_task", json!({})),
        ("submit_intent", json!({"text": 7})),
    ] {
        let (_, error) = call(&bridge, 13, name, arguments).await;
        assert!(error, "{name}");
    }
}
