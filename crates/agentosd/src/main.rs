//! `agentosd`: the AgentOS P0 daemon.
//!
//! Reads intents as lines from stdin (standing in for keyboard / speech),
//! lets the butler agent act on them, and prints what it says to the console.
//! Every step is appended to the audit log.

use std::sync::Arc;

use agentos_audit::AuditLog;
use agentos_core::{AgentId, Sensor};
use agentos_policy::PolicyEngine;
use agentosd::builtin;
use agentosd::butler::{grant_manifest, Butler};
use agentosd::devices::{ConsoleOutput, LineSensor};
use agentosd::expression::ExpressionHub;
use agentosd::perception::PerceptionHub;
use agentosd::planner::RulePlanner;
use agentosd::registry::CapabilityRegistry;
use tokio::sync::mpsc;

fn audit_path() -> String {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--audit" {
            if let Some(p) = args.next() {
                return p;
            }
        }
    }
    std::env::var("AGENTOS_AUDIT").unwrap_or_else(|_| "agentos-audit.jsonl".into())
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let audit = Arc::new(AuditLog::open(audit_path())?);

    let mut registry = CapabilityRegistry::new();
    builtin::register_all(&mut registry);

    let butler_id = AgentId::new("butler");
    let mut policy = PolicyEngine::new();
    let manifest: Vec<_> = registry.specs().into_iter().map(|s| s.name).collect();
    grant_manifest(&mut policy, &butler_id, &manifest);

    let mut expr = ExpressionHub::new(audit.clone());
    expr.add_output(Arc::new(ConsoleOutput::default()));

    let mut butler = Butler::new(
        butler_id,
        registry,
        policy,
        Box::new(RulePlanner),
        audit.clone(),
        expr,
    );

    // Sensors run as independent tasks and feed one percept channel.
    let (tx, mut rx) = mpsc::channel(64);
    let mut sensor = LineSensor::stdin();
    tokio::spawn(async move {
        while let Some(p) = sensor.next().await {
            if tx.send(p).await.is_err() {
                break;
            }
        }
    });

    println!(
        "AgentOS P0 · 管家 Agent 已就绪（审计日志：{}）。输入你的意图，Ctrl-D 退出。",
        audit.path().display()
    );
    let mut perception = PerceptionHub::new();
    while let Some(p) = rx.recv().await {
        if let Some(intent) = perception.to_intent(p) {
            butler.handle(intent).await;
        }
    }
    Ok(())
}
