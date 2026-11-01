//! The butler as dispatcher: multi-round planning, delegation to sub-agents,
//! narrowed permissions, and confirmations routed back to the human.

use std::sync::{Arc, Mutex};

use agentos_audit::{AuditEvent, AuditLog};
use agentos_core::{
    now_ms, AgentId, CapabilityName, DeviceId, DeviceInfo, Expression, ExpressionKind, Intent,
    IntentId, Modality, Output, OutputError, PrivacyClass, RiskLevel, Urgency,
};
use agentos_policy::PolicyEngine;
use agentosd::agents::{AgentDirectory, AgentManifest};
use agentosd::builtin::{self, NoteStore};
use agentosd::butler::{grant_manifest, Butler};
use agentosd::expression::ExpressionHub;
use agentosd::planner::{Conversation, Plan, Planner, RulePlanner, Step};
use agentosd::registry::CapabilityRegistry;
use async_trait::async_trait;
use serde_json::json;

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

/// What a planner was shown on one call.
#[derive(Debug, Clone)]
struct Seen {
    sub_agent: bool,
    task: String,
    rounds: usize,
    tools: Vec<String>,
    last_results: Vec<String>,
}

type Script = dyn Fn(&Seen) -> Plan + Send + Sync;

/// A planner driven by a closure, recording every call.
struct Scripted {
    script: Box<Script>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

#[async_trait]
impl Planner for Scripted {
    async fn plan(&self, _: &Intent, _: &[agentos_core::CapabilitySpec]) -> Plan {
        unreachable!("the butler always calls plan_turn")
    }
    async fn plan_turn(&self, c: &Conversation<'_>) -> Plan {
        let seen = Seen {
            sub_agent: !c.system.starts_with("你是 AgentOS 的管家"),
            task: c.intent.text.clone(),
            rounds: c.history.len(),
            tools: c.available.iter().map(|s| s.name.to_string()).collect(),
            last_results: c
                .history
                .last()
                .map(|r| r.results.iter().map(|x| x.content.clone()).collect())
                .unwrap_or_default(),
        };
        self.seen.lock().unwrap().push(seen.clone());
        (self.script)(&seen)
    }
}

fn call(cap: &str, args: serde_json::Value) -> Plan {
    Plan {
        steps: vec![Step::new(CapabilityName::new(cap), args)],
        ..Plan::default()
    }
}

fn answer(text: &str) -> Plan {
    Plan {
        reply: Some(text.into()),
        ..Plan::default()
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
    fn new(name: &str, planner: Box<dyn Planner>, agents: AgentDirectory) -> Self {
        let audit_path = std::env::temp_dir().join(format!(
            "agentosd-agents-{name}-{}-{}.jsonl",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_file(&audit_path);
        let audit = Arc::new(AuditLog::open(&audit_path).unwrap());
        let mut registry = CapabilityRegistry::new();
        let notes = builtin::register_all(&mut registry);
        let id = AgentId::new("butler");
        let mut policy = PolicyEngine::new();
        let manifest: Vec<_> = registry.specs().into_iter().map(|s| s.name).collect();
        grant_manifest(&mut policy, &id, &manifest);
        let out = Arc::new(Recorder::default());
        let mut expr = ExpressionHub::new(audit.clone());
        expr.add_output(out.clone());
        let butler = Butler::new(id, registry, policy, planner, audit, expr).with_agents(agents);
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
        self.out.0.lock().unwrap().last().cloned().unwrap()
    }

    fn notes(&self) -> Vec<String> {
        self.notes.lock().unwrap().clone()
    }

    fn audit(&self) -> Vec<AuditEvent> {
        AuditLog::replay(&self.audit_path)
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect()
    }

    fn invoked_by(&self) -> Vec<(String, String)> {
        self.audit()
            .into_iter()
            .filter_map(|e| match e {
                AuditEvent::CapabilityInvoked {
                    agent, capability, ..
                } => Some((agent.to_string(), capability.to_string())),
                _ => None,
            })
            .collect()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.audit_path);
    }
}

#[tokio::test]
async fn butler_plans_again_with_the_results_of_its_calls() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let planner = Scripted {
        script: Box::new(|s: &Seen| match s.rounds {
            0 => call("notes.list", json!({})),
            1 if s.last_results[0].contains("牛奶") => {
                call("notes.append", json!({"text": "已经有牛奶了"}))
            }
            _ => answer("都处理好了"),
        }),
        seen: seen.clone(),
    };
    let mut h = Harness::new("rounds", Box::new(planner), AgentDirectory::new());
    h.notes.lock().unwrap().push("买牛奶".into());

    let r = h.say("看看笔记，有牛奶就备注一下").await;
    assert_eq!(r.text, "都处理好了");
    assert_eq!(h.notes(), vec!["买牛奶", "已经有牛奶了"]);
    assert_eq!(seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn butler_delegates_and_gets_the_agents_answer() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let planner = Scripted {
        script: Box::new(|s: &Seen| match (s.sub_agent, s.rounds) {
            (false, 0) => call("agent.notes", json!({"task": "记下：买牛奶"})),
            (true, 0) => call("notes.append", json!({"text": "买牛奶"})),
            (true, _) => answer("记好了 1 条"),
            (false, _) => answer(&format!("笔记 Agent 说：{}", s.last_results[0])),
        }),
        seen: seen.clone(),
    };
    let mut h = Harness::new(
        "delegate",
        Box::new(planner),
        AgentDirectory::with_builtin(),
    );

    let r = h.say("帮我记一下买牛奶").await;
    assert_eq!(r.text, "笔记 Agent 说：记好了 1 条");
    assert_eq!(h.notes(), vec!["买牛奶"]);
    assert_eq!(
        h.invoked_by(),
        vec![("notes".to_string(), "notes.append".to_string())]
    );

    let seen = seen.lock().unwrap();
    let butler_tools = &seen[0].tools;
    assert!(butler_tools.contains(&"agent.notes".to_string()));
    assert!(butler_tools.contains(&"agent.craftsman".to_string()));
    let sub = seen.iter().find(|s| s.sub_agent).unwrap();
    assert_eq!(sub.task, "记下：买牛奶");
    // The notes agent sees only what its manifest asks for: no evolve, no agents.
    assert!(sub
        .tools
        .iter()
        .all(|t| t.starts_with("notes.") || t.starts_with("board.") || t == "clock.now"));

    let events = h.audit();
    assert!(events.iter().any(|e| matches!(e,
        AuditEvent::Delegated { from, to, .. } if from.as_str() == "butler" && to.as_str() == "notes")));
    assert!(events.iter().any(|e| matches!(e,
        AuditEvent::AgentFinished { agent, ok: true, summary, .. }
            if agent.as_str() == "notes" && summary == "记好了 1 条")));
}

#[tokio::test]
async fn delegates_confirmations_go_through_the_butler() {
    // "@notes ..." is the rule planner's way to delegate, so this runs offline.
    let mut h = Harness::new(
        "sub-confirm",
        Box::new(RulePlanner),
        AgentDirectory::with_builtin(),
    );
    h.say("记一下 买牛奶").await;

    let r = h.say("@notes 清空笔记").await;
    assert_eq!(r.kind, ExpressionKind::AskConfirm);
    assert_eq!(r.from.as_str(), "butler");
    assert!(r.text.contains("笔记 Agent请求执行 notes.clear"));
    assert!(h.butler.awaiting_confirmation());
    assert_eq!(h.notes(), vec!["买牛奶"]);

    let r = h.say("y").await;
    assert!(!h.butler.awaiting_confirmation());
    assert!(h.notes().is_empty());
    assert_eq!(r.from.as_str(), "notes");
    assert_eq!(
        h.invoked_by().last().unwrap(),
        &("notes".to_string(), "notes.clear".to_string())
    );

    // Cancelling a delegate's step cancels the whole intent.
    h.say("记一下 牛奶").await;
    h.say("@notes 清空笔记").await;
    let r = h.say("算了").await;
    assert!(r.text.contains("计划中止"));
    assert!(!h.butler.awaiting_confirmation());
    assert_eq!(h.notes(), vec!["牛奶"]);
}

#[tokio::test]
async fn a_delegate_cannot_exceed_its_risk_ceiling() {
    let mut agents = AgentDirectory::new();
    agents
        .add(AgentManifest {
            id: "scribe".into(),
            name: "记录员".into(),
            description: "只能追加笔记".into(),
            capabilities: vec!["notes.*".into()],
            max_risk: RiskLevel::Reversible,
            instructions: None,
            max_rounds: 3,
        })
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let planner = Scripted {
        script: Box::new(|s: &Seen| match (s.sub_agent, s.rounds) {
            (false, 0) => call("agent.scribe", json!({"task": "清空笔记"})),
            // The delegate tries anyway: the policy engine has not granted it.
            (true, 0) => call("notes.clear", json!({})),
            (true, _) => answer("我没有清空笔记的权限"),
            (false, _) => answer("记录员做不到"),
        }),
        seen: seen.clone(),
    };
    let mut h = Harness::new("ceiling", Box::new(planner), agents);
    h.notes.lock().unwrap().push("买牛奶".into());

    let r = h.say("清空笔记").await;
    assert_eq!(r.text, "记录员做不到");
    assert!(!h.butler.awaiting_confirmation());
    assert_eq!(h.notes(), vec!["买牛奶"]);
    let seen = seen.lock().unwrap();
    let sub = seen.iter().find(|s| s.sub_agent).unwrap();
    assert!(!sub.tools.contains(&"notes.clear".to_string()));
    assert!(sub.tools.contains(&"notes.append".to_string()));
    let denied = seen.iter().find(|s| s.sub_agent && s.rounds == 1).unwrap();
    assert!(denied.last_results[0].contains("被策略拒绝"));
}

#[tokio::test]
async fn delegates_cannot_delegate_and_rounds_are_bounded() {
    let planner = Scripted {
        // Never stops calling tools.
        script: Box::new(|s: &Seen| {
            if s.sub_agent {
                call("agent.notes", json!({"task": "再交给自己"}))
            } else if s.rounds == 0 {
                call("agent.notes", json!({"task": "看看笔记"}))
            } else {
                answer("好")
            }
        }),
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let mut h = Harness::new("bounded", Box::new(planner), AgentDirectory::with_builtin());
    let r = h.say("看看笔记").await;
    assert_eq!(r.text, "好");
    let events = h.audit();
    let delegations = events
        .iter()
        .filter(|e| matches!(e, AuditEvent::Delegated { .. }))
        .count();
    assert_eq!(delegations, 1);
    // The notes agent ran out of rounds and said so.
    assert!(events.iter().any(|e| matches!(e,
        AuditEvent::AgentFinished { agent, summary, .. }
            if agent.as_str() == "notes" && summary.contains("轮"))));
}
