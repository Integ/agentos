//! OS concepts mapped onto the agent: independent tasks, durable checkpoints
//! that survive a restart, a shared blackboard, memory across sessions, and a
//! judge that can only tighten what policy allowed.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentos_audit::AuditLog;
use agentos_core::{
    now_ms, AgentId, Capability, CapabilityError, CapabilityName, CapabilitySpec, DeviceId,
    DeviceInfo, Expression, ExpressionKind, Intent, IntentId, Modality, Output, OutputError,
    PrivacyClass, RiskLevel, Urgency,
};
use agentos_policy::PolicyEngine;
use agentosd::agents::AgentDirectory;
use agentosd::blackboard::{self, Blackboard};
use agentosd::builtin::{self, NoteStore};
use agentosd::butler::{grant_manifest, Butler};
use agentosd::expression::ExpressionHub;
use agentosd::judge::{Judge, Review, Verdict};
use agentosd::planner::{Conversation, Plan, Planner, RulePlanner, Step};
use agentosd::registry::CapabilityRegistry;
use agentosd::tasks::TaskStore;
use async_trait::async_trait;
use serde_json::{json, Value};

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

/// Never finishes: stands for a call cut off by a crash.
struct Hang(CapabilitySpec);

#[async_trait]
impl Capability for Hang {
    fn spec(&self) -> &CapabilitySpec {
        &self.0
    }
    async fn invoke(&self, _: Value) -> Result<Value, CapabilityError> {
        std::future::pending().await
    }
}

#[derive(Debug, Clone)]
struct Seen {
    system: String,
    task: String,
    rounds: usize,
    last_results: Vec<String>,
}

type Script = dyn Fn(&Seen) -> Plan + Send + Sync;

struct Scripted {
    script: Arc<Script>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

#[async_trait]
impl Planner for Scripted {
    async fn plan(&self, _: &Intent, _: &[CapabilitySpec]) -> Plan {
        unreachable!()
    }
    async fn plan_turn(&self, c: &Conversation<'_>) -> Plan {
        let seen = Seen {
            system: c.system.to_string(),
            task: c.intent.text.clone(),
            rounds: c.history.len(),
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

fn call(cap: &str, args: Value) -> Plan {
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

/// Flags every notes.append.
struct NervousJudge(Mutex<Vec<String>>);

#[async_trait]
impl Judge for NervousJudge {
    async fn review(&self, r: &Review<'_>) -> Result<Verdict, String> {
        self.0.lock().unwrap().push(r.spec.name.to_string());
        Ok(if r.spec.name.as_str() == "notes.append" {
            Verdict::Flag("笔记内容像是密码".into())
        } else {
            Verdict::Allow
        })
    }
}

struct Home(PathBuf);

impl Home {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "agentosd-tasks-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    butler: Butler,
    out: Arc<Recorder>,
    notes: NoteStore,
    next: u64,
}

impl Harness {
    /// A butler living in `home`: tasks and blackboard persist there.
    fn new(home: &Home, planner: Box<dyn Planner>, judge: Option<Arc<dyn Judge>>) -> Self {
        let audit = Arc::new(AuditLog::open(home.0.join("audit.jsonl")).unwrap());
        let mut registry = CapabilityRegistry::new();
        let notes = builtin::register_all(&mut registry);
        let board = Blackboard::open(home.0.join("blackboard.json")).unwrap();
        blackboard::register_all(&mut registry, board.clone());
        registry.register(Arc::new(Hang(CapabilitySpec {
            name: CapabilityName::new("test.hang"),
            description: "never returns".into(),
            risk: RiskLevel::Reversible,
            input_schema: json!({}),
        })));
        let id = AgentId::new("butler");
        let mut policy = PolicyEngine::new();
        let manifest: Vec<_> = registry.specs().into_iter().map(|s| s.name).collect();
        grant_manifest(&mut policy, &id, &manifest);
        let out = Arc::new(Recorder::default());
        let mut expr = ExpressionHub::new(audit.clone());
        expr.add_output(out.clone());
        let mut butler = Butler::new(id, registry, policy, planner, audit, expr)
            .with_agents(AgentDirectory::with_builtin())
            .with_blackboard(board);
        if let Some(j) = judge {
            butler = butler.with_judge(j);
        }
        let (butler, notes_) =
            butler.with_task_store(TaskStore::open(home.0.join("tasks")).unwrap());
        assert!(notes_.is_empty(), "{notes_:?}");
        Self {
            butler,
            out,
            notes,
            next: 0,
        }
    }

    fn intent(&mut self, text: &str) -> Intent {
        self.next += 1;
        Intent {
            id: IntentId(self.next),
            source: DeviceId::new("test"),
            speaker: Some("adele".into()),
            text: text.into(),
            urgency: Urgency::Normal,
            at_ms: now_ms(),
        }
    }

    async fn say(&mut self, text: &str) -> Expression {
        let i = self.intent(text);
        self.butler.handle(i).await;
        self.last()
    }

    fn last(&self) -> Expression {
        self.out.0.lock().unwrap().last().cloned().unwrap()
    }

    fn notes(&self) -> Vec<String> {
        self.notes.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn a_waiting_task_does_not_block_the_next_one() {
    let home = Home::new("multi");
    let mut h = Harness::new(&home, Box::new(RulePlanner), None);
    h.say("记一下 买牛奶").await;

    let r = h.say("@notes 清空笔记").await;
    assert_eq!(r.kind, ExpressionKind::AskConfirm);
    assert!(r.text.starts_with("任务 #2"));

    // Not an answer: a new task runs while #2 keeps waiting.
    h.say("记一下 买鸡蛋").await;
    assert_eq!(h.notes(), vec!["买牛奶", "买鸡蛋"]);
    assert_eq!(h.butler.waiting_tasks(), vec![2]);

    let r = h.say("清空笔记").await;
    assert!(r.text.starts_with("任务 #4"));
    assert_eq!(h.butler.waiting_tasks(), vec![2, 4]);

    let r = h.say("任务").await;
    assert!(r.text.contains("#2 等待确认"), "{}", r.text);
    assert!(r.text.contains("#4 等待确认"));
    assert!(r.text.contains("#3 已完成"));

    // "n" answers the newest; "y #2" names the older one.
    let r = h.say("n").await;
    assert!(r.text.contains("任务 #4"));
    assert_eq!(h.butler.waiting_tasks(), vec![2]);
    h.say("y #2").await;
    assert!(h.notes().is_empty());
    assert!(!h.butler.awaiting_confirmation());
}

#[tokio::test]
async fn one_task_can_cancel_another() {
    let home = Home::new("cancel");
    let mut h = Harness::new(&home, Box::new(RulePlanner), None);
    h.say("记一下 买牛奶").await;
    h.say("清空笔记").await;
    assert_eq!(h.butler.waiting_tasks(), vec![2]);
    let r = h.say("取消任务 2").await;
    assert!(r.text.contains("已取消任务 #2"), "{}", r.text);
    assert!(!h.butler.awaiting_confirmation());
    assert_eq!(h.notes(), vec!["买牛奶"]);
}

#[tokio::test]
async fn waiting_tasks_survive_a_restart() {
    let home = Home::new("restart-wait");
    {
        let mut h = Harness::new(&home, Box::new(RulePlanner), None);
        h.say("@notes 清空笔记").await;
        assert!(h.butler.awaiting_confirmation());
    } // agentosd stops

    let mut h = Harness::new(&home, Box::new(RulePlanner), None);
    h.notes.lock().unwrap().push("新的笔记".into());
    assert_eq!(h.butler.pending_resume(), 1);
    h.butler.resume().await;
    let r = h.last();
    assert_eq!(r.kind, ExpressionKind::AskConfirm);
    assert!(r.text.contains("重启前未完成的任务 #1"), "{}", r.text);
    assert!(r.text.contains("笔记 Agent请求执行 notes.clear"));

    h.say("y").await;
    assert!(h.notes().is_empty());
    // New tasks continue the numbering.
    let r = h.say("@notes 清空笔记").await;
    assert!(r.text.starts_with("任务 #2"), "{}", r.text);
}

#[tokio::test]
async fn a_call_cut_off_by_a_crash_is_reported_not_rerun() {
    let home = Home::new("restart-run");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let script: Arc<Script> = Arc::new(|s: &Seen| match s.rounds {
        0 => call("test.hang", json!({})),
        _ => answer(&format!("上一步：{}", s.last_results[0])),
    });
    {
        let mut h = Harness::new(
            &home,
            Box::new(Scripted {
                script: script.clone(),
                seen: seen.clone(),
            }),
            None,
        );
        let i = h.intent("做一件会被打断的事");
        // The process dies while the call is running.
        let cut = tokio::time::timeout(Duration::from_millis(100), h.butler.handle(i)).await;
        assert!(cut.is_err());
    }

    let mut h = Harness::new(
        &home,
        Box::new(Scripted {
            script,
            seen: seen.clone(),
        }),
        None,
    );
    h.butler.resume().await;
    let r = h.last();
    assert!(r.text.contains("中断"), "{}", r.text);
    assert!(!h.butler.awaiting_confirmation());
    // Planned once before the crash, once after (with the interrupted result).
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn context_carries_across_tasks_and_restarts() {
    let home = Home::new("memory");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let script: Arc<Script> = Arc::new(|s: &Seen| match (s.task.as_str(), s.rounds) {
        ("下周去杭州", 0) => call(
            "board.write",
            json!({"op": "set", "path": "trip/city", "value": "杭州"}),
        ),
        ("下周去杭州", _) => answer("记住了：下周去杭州"),
        _ => answer("好的"),
    });
    {
        let mut h = Harness::new(
            &home,
            Box::new(Scripted {
                script: script.clone(),
                seen: seen.clone(),
            }),
            None,
        );
        h.say("下周去杭州").await;
        h.say("天气怎么样").await;
    }
    let mut h = Harness::new(
        &home,
        Box::new(Scripted {
            script,
            seen: seen.clone(),
        }),
        None,
    );
    h.say("帮我准备行李").await;

    let seen = seen.lock().unwrap();
    let before = &seen[2].system; // "天气怎么样", same session
    let after = &seen[3].system; // "帮我准备行李", after a restart
    for s in [before, after] {
        assert!(
            s.contains("#1 已完成：下周去杭州 → 记住了：下周去杭州"),
            "{s}"
        );
        assert!(s.contains("\"city\":\"杭州\""), "{s}");
    }
    assert!(after.contains("#2 已完成：天气怎么样"));
}

#[tokio::test]
async fn the_judge_can_only_ask_for_confirmation() {
    let home = Home::new("judge");
    let judge = Arc::new(NervousJudge(Mutex::new(Vec::new())));
    let mut h = Harness::new(&home, Box::new(RulePlanner), Some(judge.clone()));

    // L0 steps are not reviewed.
    h.say("看看笔记").await;
    assert!(judge.0.lock().unwrap().is_empty());

    // An L1 step the policy engine would allow is flagged: the human decides.
    let r = h.say("记一下 密码是 hunter2").await;
    assert_eq!(r.kind, ExpressionKind::AskConfirm);
    assert!(r.text.contains("裁判提醒：笔记内容像是密码"), "{}", r.text);
    assert!(h.notes().is_empty());
    h.say("y").await;
    assert_eq!(h.notes(), vec!["密码是 hunter2"]);

    // Allowed steps run without asking.
    let r = h.say("现在几点").await;
    assert_eq!(r.kind, ExpressionKind::Report);
}
