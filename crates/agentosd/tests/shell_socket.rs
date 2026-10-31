//! End-to-end over the shell socket: a client sends intents and confirmations
//! as JSON lines and receives structured progress events.

use std::sync::Arc;
use std::time::Duration;

use agentos_audit::AuditLog;
use agentos_core::{now_ms, AgentId};
use agentos_policy::PolicyEngine;
use agentosd::builtin;
use agentosd::butler::{grant_manifest, Butler};
use agentosd::expression::ExpressionHub;
use agentosd::perception::PerceptionHub;
use agentosd::planner::RulePlanner;
use agentosd::registry::CapabilityRegistry;
use agentosd::shell_api::{self, ShellServer, SocketOutput};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

async fn next_of(lines: &mut Lines<BufReader<OwnedReadHalf>>, ty: &str) -> Value {
    loop {
        let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("timed out waiting for event")
            .unwrap()
            .expect("socket closed");
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["type"] == ty {
            return v;
        }
    }
}

#[tokio::test]
async fn shell_drives_the_butler_over_the_socket() {
    let dir = std::env::temp_dir().join(format!(
        "agentosd-shell-{}-{}",
        std::process::id(),
        now_ms()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let audit_path = dir.join("audit.jsonl");
    let sock = dir.join("agentos.sock");

    let audit = Arc::new(AuditLog::open(&audit_path).unwrap());
    let mut registry = CapabilityRegistry::new();
    builtin::register_all(&mut registry);
    let id = AgentId::new("butler");
    let mut policy = PolicyEngine::new();
    let manifest: Vec<_> = registry.specs().into_iter().map(|s| s.name).collect();
    grant_manifest(&mut policy, &id, &manifest);

    let bus = shell_api::event_bus();
    let mut expr = ExpressionHub::new(audit.clone());
    expr.add_output(Arc::new(SocketOutput::new(bus.clone())));
    let mut butler = Butler::new(
        id,
        registry,
        policy,
        Box::new(RulePlanner),
        audit.clone(),
        expr,
    )
    .with_events(bus.clone());

    let (tx, mut rx) = mpsc::channel(8);
    let listener = shell_api::bind(&sock).unwrap();
    tokio::spawn(
        ShellServer {
            hello: json!({"type": "hello", "planner": "规则规划器"}),
            bus,
            audit: audit_path.clone(),
            percepts: tx,
            voice: None,
        }
        .serve(listener),
    );
    tokio::spawn(async move {
        let mut perception = PerceptionHub::new();
        while let Some(p) = rx.recv().await {
            if let Some(i) = perception.to_intent(p) {
                butler.handle(i).await;
            }
        }
    });

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    assert_eq!(next_of(&mut lines, "hello").await["planner"], "规则规划器");
    next_of(&mut lines, "activity").await;

    let send = |v: Value| format!("{v}\n");
    write
        .write_all(send(json!({"type": "intent", "text": "记一下 买牛奶"})).as_bytes())
        .await
        .unwrap();
    let plan = next_of(&mut lines, "plan").await;
    assert_eq!(plan["steps"][0]["capability"], "notes.append");
    assert_eq!(plan["steps"][0]["risk"], "L1");
    let step = next_of(&mut lines, "step").await;
    assert_eq!(step["status"], "running");
    let step = next_of(&mut lines, "step").await;
    assert_eq!(step["status"], "done");
    next_of(&mut lines, "done").await;

    // An irreversible step pauses for a confirmation the shell answers.
    write
        .write_all(send(json!({"type": "intent", "text": "清空笔记"})).as_bytes())
        .await
        .unwrap();
    let confirm = next_of(&mut lines, "confirm").await;
    assert_eq!(confirm["capability"], "notes.clear");
    assert_eq!(confirm["risk"], "L3");
    write
        .write_all(send(json!({"type": "confirm", "approved": true})).as_bytes())
        .await
        .unwrap();
    let step = next_of(&mut lines, "step").await;
    assert_eq!(step["status"], "running");
    next_of(&mut lines, "done").await;

    // A new client sees what happened in the activity feed.
    let stream = UnixStream::connect(&sock).await.unwrap();
    let (read, _w) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let activity = next_of(&mut lines, "activity").await;
    let caps: Vec<_> = activity["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["capability"].as_str().unwrap().to_string())
        .collect();
    assert!(caps.contains(&"notes.append".to_string()));
    assert!(caps.contains(&"notes.clear".to_string()));

    let _ = std::fs::remove_dir_all(&dir);
}
