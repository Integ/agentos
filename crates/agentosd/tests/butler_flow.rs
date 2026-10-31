//! End-to-end: intents in, expressions and audit entries out.

use std::sync::{Arc, Mutex};

use agentos_audit::{AuditEvent, AuditLog};
use agentos_core::{
    now_ms, AgentId, CapabilityName, DeviceId, DeviceInfo, Expression, ExpressionKind, Intent,
    IntentId, Modality, Output, OutputError, PrivacyClass, Urgency,
};
use agentos_policy::PolicyEngine;
use agentosd::builtin::{self, NoteStore};
use agentosd::butler::{grant_manifest, Butler};
use agentosd::expression::ExpressionHub;
use agentosd::planner::RulePlanner;
use agentosd::registry::CapabilityRegistry;
use async_trait::async_trait;

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
        let audit_path = std::env::temp_dir().join(format!(
            "agentosd-{name}-{}-{}.jsonl",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_file(&audit_path);
        let audit = Arc::new(AuditLog::open(&audit_path).unwrap());
        let mut registry = CapabilityRegistry::new();
        let notes = builtin::register_all(&mut registry);
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
        let butler = Butler::new(id, registry, policy, Box::new(RulePlanner), audit, expr);
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
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.audit_path);
    }
}

#[tokio::test]
async fn irreversible_step_waits_for_confirmation() {
    let mut h = Harness::new("confirm", |_| true);

    let r = h.say("记一下 买牛奶").await;
    assert_eq!(r.kind, ExpressionKind::Report);
    assert_eq!(h.notes(), vec!["买牛奶"]);

    let r = h.say("清空笔记").await;
    assert_eq!(r.kind, ExpressionKind::AskConfirm);
    assert!(h.butler.awaiting_confirmation());

    // Anything but yes cancels.
    let r = h.say("算了").await;
    assert_eq!(r.kind, ExpressionKind::Report);
    assert!(!h.butler.awaiting_confirmation());
    assert_eq!(h.notes(), vec!["买牛奶"]);

    h.say("清空笔记").await;
    let r = h.say("y").await;
    assert_eq!(r.kind, ExpressionKind::Report);
    assert!(h.notes().is_empty());

    let entries = AuditLog::replay(&h.audit_path).unwrap();
    let confirmations: Vec<bool> = entries
        .iter()
        .filter_map(|e| match &e.event {
            AuditEvent::HumanConfirmation { approved, .. } => Some(*approved),
            _ => None,
        })
        .collect();
    assert_eq!(confirmations, vec![false, true]);
    assert!(entries.windows(2).all(|w| w[1].seq == w[0].seq + 1));
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
}
