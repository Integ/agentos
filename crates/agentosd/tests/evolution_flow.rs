//! End-to-end self-evolution: the human asks for a new ability, the system
//! writes and tests a plugin, the human approves, the butler can use it, and
//! the change can be rolled back.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentos_audit::{AuditEvent, AuditLog};
use agentos_core::{
    now_ms, AgentId, CapabilityName, CapabilitySpec, DeviceId, DeviceInfo, Expression,
    ExpressionKind, Intent, IntentId, Modality, Output, OutputError, PrivacyClass, RiskLevel,
    Urgency,
};
use agentos_policy::PolicyEngine;
use agentosd::builtin;
use agentosd::butler::{grant_manifest, Butler};
use agentosd::evolve::{self, GenerationRequest, PluginGenerator};
use agentosd::expression::ExpressionHub;
use agentosd::planner::{Plan, Planner, RulePlanner, Step};
use agentosd::plugin::{Permissions, PluginManifest, SandboxMode, TestCase};
use agentosd::plugin_store::{PluginDraft, PluginManager};
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

/// "call <capability>" invokes it directly; everything else goes to the rule planner.
struct TestPlanner;

#[async_trait]
impl Planner for TestPlanner {
    async fn plan(&self, intent: &Intent, available: &[CapabilitySpec]) -> Plan {
        if let Some(name) = intent.text.strip_prefix("call ") {
            return Plan {
                steps: vec![Step {
                    capability: CapabilityName::new(name.trim()),
                    args: json!({}),
                }],
                ..Plan::default()
            };
        }
        RulePlanner.plan(intent, available).await
    }
}

/// Writes a greeting plugin. The first attempt of each request is broken,
/// so the self-test feedback loop is exercised.
struct FakeGenerator {
    calls: AtomicUsize,
    greeting: Mutex<String>,
}

#[async_trait]
impl PluginGenerator for FakeGenerator {
    async fn generate(&self, req: &GenerationRequest) -> Result<PluginDraft, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let greeting = self.greeting.lock().unwrap().clone();
        let source = match &req.retry {
            None => "echo 'not json'\n".to_string(),
            Some((_, why)) => {
                assert!(why.contains("自测失败"), "{why}");
                format!("printf '{{\"message\":\"{greeting}\"}}'\n")
            }
        };
        Ok(PluginDraft {
            manifest: PluginManifest {
                name: CapabilityName::new("greet.hello"),
                description: "打个招呼".into(),
                risk: RiskLevel::ReadOnly,
                input_schema: json!({"type": "object", "properties": {}}),
                permissions: Permissions::default(),
                runtime: "sh".into(),
                summary: "输出一句问候".into(),
                tests: vec![TestCase {
                    args: json!({}),
                    expect_contains: Some(greeting),
                }],
                need: req.need.clone(),
            },
            source,
        })
    }
}

struct Harness {
    butler: Butler,
    out: Arc<Recorder>,
    gen: Arc<FakeGenerator>,
    pm: Arc<PluginManager>,
    audit_path: std::path::PathBuf,
    next: u64,
}

impl Harness {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "agentos-evo-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let audit_path = base.join("audit.jsonl");
        let audit = Arc::new(AuditLog::open(&audit_path).unwrap());
        let pm = Arc::new(
            PluginManager::open(base.join("evolve"), SandboxMode::detect())
                .unwrap()
                .allow_unsandboxed_tests(true),
        );
        let gen = Arc::new(FakeGenerator {
            calls: AtomicUsize::new(0),
            greeting: Mutex::new("你好，Adele".into()),
        });
        let mut registry = CapabilityRegistry::new();
        builtin::register_all(&mut registry);
        evolve::register_all(&mut registry, pm.clone(), gen.clone());
        let id = AgentId::new("butler");
        let mut policy = PolicyEngine::new();
        let manifest: Vec<_> = registry.specs().into_iter().map(|s| s.name).collect();
        grant_manifest(&mut policy, &id, &manifest);
        let out = Arc::new(Recorder::default());
        let mut expr = ExpressionHub::new(audit.clone());
        expr.add_output(out.clone());
        let butler = Butler::new(id, registry, policy, Box::new(TestPlanner), audit, expr)
            .with_plugins(pm.clone());
        Self {
            butler,
            out,
            gen,
            pm,
            audit_path,
            next: 0,
        }
    }

    /// Say something; return everything the butler expressed in response.
    async fn say(&mut self, text: &str) -> Vec<Expression> {
        let before = self.out.0.lock().unwrap().len();
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
        self.out.0.lock().unwrap()[before..].to_vec()
    }

    /// Call a plugin. Without a sandbox every plugin call needs confirmation
    /// (effective risk L2), so confirm it.
    async fn call(&mut self, name: &str) -> Vec<Expression> {
        let r = self.say(&format!("call {name}")).await;
        if last(&r).kind == ExpressionKind::AskConfirm {
            assert_eq!(self.pm.sandbox(), SandboxMode::None);
            return self.say("y").await;
        }
        r
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(base) = self.audit_path.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

fn last(v: &[Expression]) -> &Expression {
    v.last().expect("no expression")
}

#[tokio::test]
async fn learn_review_install_use_and_roll_back() {
    let mut h = Harness::new("full");

    // Unknown before evolution.
    let r = h.say("call greet.hello").await;
    assert_eq!(last(&r).kind, ExpressionKind::Error);

    // Ask for a new ability: draft is written, retried after a failed self-test,
    // shown for review, and installation waits for confirmation.
    let r = h.say("学会：跟我打招呼").await;
    assert_eq!(h.gen.calls.load(Ordering::SeqCst), 2);
    let review = r
        .iter()
        .find(|e| e.text.contains("新能力草案"))
        .expect("draft shown for review");
    assert!(review.text.contains("greet.hello"));
    assert!(review.text.contains("printf"), "code is shown");
    assert!(review.text.contains("1/1 通过") || review.text.contains("未运行"));
    assert_eq!(last(&r).kind, ExpressionKind::AskConfirm);
    assert!(last(&r).text.contains("evolve.install"));
    assert!(h.pm.list().is_empty(), "nothing installed before approval");

    let r = h.say("y").await;
    assert!(
        last(&r).text.contains("已安装 greet.hello v1"),
        "{:?}",
        last(&r)
    );

    // The butler can use it right away.
    let r = h.call("greet.hello").await;
    assert_eq!(last(&r).kind, ExpressionKind::Report);
    assert_eq!(last(&r).text, "你好，Adele");

    // Upgrade: a diff is shown, then v2 replaces v1.
    *h.gen.greeting.lock().unwrap() = "早上好，Adele".into();
    let r = h.say("学会：换成早上好").await;
    assert!(r.iter().any(|e| e.text.contains("与当前 v1 的差异")));
    h.say("y").await;
    let r = h.call("greet.hello").await;
    assert_eq!(last(&r).text, "早上好，Adele");

    // Roll back to v1.
    let r = h.say("回滚 greet.hello").await;
    assert!(last(&r).text.contains("v2 切换到 v1"), "{:?}", last(&r));
    let r = h.call("greet.hello").await;
    assert_eq!(last(&r).text, "你好，Adele");

    // Disable: the capability disappears, and comes back with 恢复.
    h.say("停用 greet.hello").await;
    let r = h.say("call greet.hello").await;
    assert_eq!(last(&r).kind, ExpressionKind::Error);
    h.say("恢复 greet.hello").await;
    let r = h.call("greet.hello").await;
    assert_eq!(last(&r).kind, ExpressionKind::Report);

    // The audit log records the approval and the digest of what was installed.
    let entries = AuditLog::replay(&h.audit_path).unwrap();
    let installs: Vec<_> = entries
        .iter()
        .filter_map(|e| match &e.event {
            AuditEvent::CapabilityInvoked {
                capability,
                result,
                ok: true,
                ..
            } if capability.as_str() == "evolve.install" => Some(result.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(installs.len(), 2);
    assert_eq!(installs[0]["digest"].as_str().unwrap().len(), 64);
    let approvals = entries
        .iter()
        .filter(|e| {
            matches!(&e.event, AuditEvent::HumanConfirmation { capability, approved: true, .. }
                if capability.as_str() == "evolve.install")
        })
        .count();
    assert_eq!(approvals, 2);
}

#[tokio::test]
async fn declined_draft_is_not_installed() {
    let mut h = Harness::new("decline");
    let r = h.say("学会：跟我打招呼").await;
    assert_eq!(last(&r).kind, ExpressionKind::AskConfirm);
    let r = h.say("不要").await;
    assert!(last(&r).text.contains("已取消"));
    assert!(h.pm.list().is_empty());
    let r = h.say("call greet.hello").await;
    assert_eq!(last(&r).kind, ExpressionKind::Error);
}

#[tokio::test]
async fn plugins_cannot_take_builtin_names() {
    let h = Harness::new("reserved");
    assert!(h.pm.is_reserved("notes.clear"));
    assert!(h.pm.is_reserved("evolve.install"));
    assert!(!h.pm.is_reserved("greet.hello"));
}
