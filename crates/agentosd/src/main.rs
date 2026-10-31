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
use agentosd::claude_planner::{ClaudeConfig, ClaudePlanner};
use agentosd::devices::{ConsoleOutput, LineSensor};
use agentosd::expression::ExpressionHub;
use agentosd::perception::PerceptionHub;
use agentosd::planner::{Planner, RulePlanner};
use agentosd::registry::CapabilityRegistry;
use tokio::sync::mpsc;

fn arg(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == name {
            return args.next();
        }
    }
    None
}

fn audit_path() -> String {
    arg("--audit")
        .or_else(|| std::env::var("AGENTOS_AUDIT").ok())
        .unwrap_or_else(|| "agentos-audit.jsonl".into())
}

/// `--planner claude|rule`. Default: Claude when `ANTHROPIC_API_KEY` is set,
/// otherwise the offline rule planner.
fn make_planner() -> (Box<dyn Planner>, String) {
    let choice = arg("--planner").or_else(|| std::env::var("AGENTOS_PLANNER").ok());
    let cfg = ClaudeConfig::from_env();
    match (choice.as_deref(), cfg) {
        (Some("rule"), _) => (Box::new(RulePlanner), "规则规划器".into()),
        (_, Some(cfg)) => match ClaudePlanner::new(cfg) {
            Ok(p) => {
                let label = format!("Claude 规划器（{}）", p.model());
                (Box::new(p), label)
            }
            Err(e) => {
                eprintln!("无法创建 HTTP 客户端：{e}，改用规则规划器");
                (Box::new(RulePlanner), "规则规划器".into())
            }
        },
        (Some("claude"), None) => {
            eprintln!("未设置 ANTHROPIC_API_KEY，改用规则规划器");
            (Box::new(RulePlanner), "规则规划器".into())
        }
        (_, None) => (Box::new(RulePlanner), "规则规划器".into()),
    }
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

    let (planner, planner_label) = make_planner();
    let mut butler = Butler::new(butler_id, registry, policy, planner, audit.clone(), expr);

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
        "AgentOS P0 · 管家 Agent 已就绪（{planner_label}，审计日志：{}）。输入你的意图，Ctrl-D 退出。",
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
