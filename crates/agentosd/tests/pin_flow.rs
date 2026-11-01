//! L3 approvals need a fresh PIN once one is set.

use std::sync::{Arc, Mutex};

use agentos_audit::AuditLog;
use agentos_core::{
    now_ms, AgentId, DeviceId, DeviceInfo, Expression, ExpressionKind, Intent, IntentId, Modality,
    Output, OutputError, PrivacyClass, Urgency,
};
use agentos_policy::PolicyEngine;
use agentosd::builtin::{self, NoteStore};
use agentosd::butler::{grant_manifest, Butler};
use agentosd::expression::ExpressionHub;
use agentosd::identity::{PinCheck, PinVault};
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

struct Harness {
    butler: Butler,
    out: Arc<Recorder>,
    notes: NoteStore,
    pins: Arc<PinVault>,
    dir: std::path::PathBuf,
    next: u64,
}

impl Harness {
    fn new(name: &str, pin: Option<&str>) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "agentosd-pin-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let audit = Arc::new(AuditLog::open(dir.join("audit.jsonl")).unwrap());
        let pins = Arc::new(
            PinVault::open(dir.join("pin.json"))
                .unwrap()
                .with_audit(audit.clone()),
        );
        if let Some(p) = pin {
            pins.set(p).unwrap();
        }
        let mut registry = CapabilityRegistry::new();
        let notes = builtin::register_all(&mut registry);
        let id = AgentId::new("butler");
        let mut policy = PolicyEngine::new();
        let manifest: Vec<_> = registry.specs().into_iter().map(|s| s.name).collect();
        grant_manifest(&mut policy, &id, &manifest);
        let out = Arc::new(Recorder::default());
        let mut expr = ExpressionHub::new(audit.clone());
        expr.add_output(out.clone());
        let butler = Butler::new(id, registry, policy, Box::new(RulePlanner), audit, expr)
            .with_pin_vault(pins.clone());
        Self {
            butler,
            out,
            notes,
            pins,
            dir,
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
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn without_a_pin_a_plain_yes_still_works_but_says_so() {
    let mut h = Harness::new("unset", None);
    h.say("记一下 买牛奶").await;
    let ask = h.say("清空笔记").await;
    assert_eq!(ask.kind, ExpressionKind::AskConfirm);
    assert!(ask.text.contains("尚未设置 PIN"), "{}", ask.text);
    h.say("y").await;
    assert!(h.notes().is_empty());
}

#[tokio::test]
async fn with_a_pin_a_plain_yes_is_not_enough() {
    let mut h = Harness::new("set", Some("2468"));
    h.say("记一下 买牛奶").await;
    let ask = h.say("清空笔记").await;
    assert!(ask.text.contains("需要输入 PIN"), "{}", ask.text);
    let task = h.butler.waiting_tasks()[0];

    let r = h.say("y").await;
    assert_eq!(r.kind, ExpressionKind::AskConfirm);
    assert!(r.text.contains("需要 PIN"), "{}", r.text);
    assert_eq!(h.notes(), vec!["买牛奶"], "nothing deleted without the PIN");
    assert!(h.butler.awaiting_confirmation());

    // A wrong PIN grants nothing.
    assert!(matches!(
        h.pins.verify_for(Some(task), "1111"),
        PinCheck::Wrong { .. }
    ));
    h.say("y").await;
    assert_eq!(h.notes(), vec!["买牛奶"]);

    // The right PIN (sent by the shell with the approval) lets it through.
    assert_eq!(h.pins.verify_for(Some(task), "2468"), PinCheck::Ok);
    h.say("y").await;
    assert!(h.notes().is_empty());
    assert!(!h.butler.awaiting_confirmation());
}

#[tokio::test]
async fn pin_is_only_needed_for_irreversible_steps() {
    let mut h = Harness::new("l1", Some("2468"));
    let r = h.say("记一下 买牛奶").await;
    assert_eq!(r.kind, ExpressionKind::Report);
    assert_eq!(h.notes(), vec!["买牛奶"]);
}

#[tokio::test]
async fn refusing_needs_no_pin() {
    let mut h = Harness::new("refuse", Some("2468"));
    h.say("记一下 买牛奶").await;
    h.say("清空笔记").await;
    let r = h.say("取消").await;
    assert_eq!(r.kind, ExpressionKind::Report);
    assert!(!h.butler.awaiting_confirmation());
    assert_eq!(h.notes(), vec!["买牛奶"]);
}
