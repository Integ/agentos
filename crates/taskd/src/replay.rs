//! Replay: put past intents to the system as it is now and compare how they
//! end with how they ended then.
//!
//! This is stage 1 of self-improvement (docs/self-improvement-research.md):
//! the evaluator. Every later stage proposes changes; this is what says
//! whether a change made things better or worse, and it is useful on its own
//! as a regression check before an upgrade.
//!
//! The rule that makes it safe to run at any time: **nothing real happens.**
//! Each case runs on a fresh butler with an in-memory task store and its own
//! scratch audit log, and every capability is replaced by a stand-in that
//! answers from the recording of that task. A call the recording does not
//! hold is never passed on to the real capability: it fails, and the case is
//! marked as having left the recording, because what would have happened is
//! unknown. Confirmations are approved automatically for the same reason:
//! approving a stand-in changes nothing.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use agentos_audit::AuditLog;
use agentos_core::{AgentId, Capability, CapabilityError, CapabilityName, CapabilitySpec, Intent};
use agentos_planner::{Conversation, Plan, Planner};
use agentos_policy::PolicyEngine;
use agentos_runtime::{StepState, Task, TaskId, TaskState};
use agentos_services::agents::{AgentDirectory, DELEGATE_PREFIX};
use agentos_services::expression::ExpressionHub;
use agentos_services::identity::PinVault;
use agentos_services::registry::CapabilityRegistry;
use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

use crate::butler::{grant_manifest, Budget, Butler, ASK_USER};
use crate::learned::{Learned, LearnedStore};

/// The PIN of a replay's own throwaway vault, so irreversible steps are
/// approved like any other. It guards nothing: the vault is deleted with the
/// case, and the capabilities behind it are stand-ins.
const REPLAY_PIN: &str = "000000";
/// Pauses (confirmations and questions) one case may go through before it
/// is stopped; a planner that keeps asking must not keep the replay going.
const MAX_PAUSES: usize = 32;
const MAX_QUOTE_CHARS: usize = 60;
const MAX_CALL_CHARS: usize = 120;

/// One call as it happened, and whether a replay has used it yet.
#[derive(Debug, Clone)]
struct Recording {
    capability: String,
    args: Value,
    ok: bool,
    result: Value,
    used: bool,
}

/// A past task to put to the system again.
#[derive(Debug, Clone)]
pub struct Case {
    pub task: TaskId,
    pub intent: Intent,
    pub outcome: TaskState,
    pub steps: usize,
    pub tokens: u64,
    tape: Vec<Recording>,
    /// What the user answered, in the order the task asked.
    answers: Vec<String>,
}

/// Tasks left out of the replay, and why.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Skipped {
    /// Cancelled, interrupted or not over: there is no outcome to compare.
    pub no_outcome: usize,
    /// Read outside content. Its recorded results would be fed to the
    /// planner again, and they are not the user's words.
    pub outside_content: usize,
    /// Submitted by an outside caller, which plans with fewer capabilities.
    pub caller: usize,
    /// Continued a conversation: it was planned with earlier tasks as
    /// context, which a replay does not have.
    pub in_thread: usize,
    /// Used a capability the replay cannot offer (an MCP tool, the smart
    /// home): the planner would not even be shown it.
    pub unavailable: usize,
    /// Asked for by number but not in the history.
    pub not_found: usize,
}

impl Skipped {
    pub fn total(&self) -> usize {
        self.no_outcome
            + self.outside_content
            + self.caller
            + self.in_thread
            + self.unavailable
            + self.not_found
    }
}

/// Which past tasks to replay.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    /// The latest this many that can be replayed.
    Latest(usize),
    /// These, in this order.
    Tasks(Vec<TaskId>),
}

/// Why a task cannot be replayed, if it cannot.
fn unsuitable(task: &Task, offers: &dyn Fn(&str) -> bool, skipped: &mut Skipped) -> bool {
    let counter = if !matches!(
        task.state,
        TaskState::Succeeded | TaskState::Unhandled | TaskState::Failed
    ) {
        &mut skipped.no_outcome
    } else if task.tainted {
        &mut skipped.outside_content
    } else if task.intent.source.to_string().starts_with("caller:") {
        &mut skipped.caller
    } else if task.thread != task.id {
        &mut skipped.in_thread
    } else if task
        .steps
        .iter()
        .any(|step| !offers(step.capability.as_str()))
    {
        &mut skipped.unavailable
    } else {
        return false;
    };
    *counter += 1;
    true
}

fn case(task: &Task) -> Case {
    let mut tape = Vec::new();
    let mut answers = Vec::new();
    for step in &task.steps {
        let name = step.capability.as_str();
        let Some(result) = step.result.clone() else {
            continue;
        };
        if name == ASK_USER {
            if step.state == StepState::Succeeded {
                answers.push(
                    result
                        .as_str()
                        .map_or_else(|| result.to_string(), str::to_string),
                );
            }
            continue;
        }
        // Delegation is the butler's own doing, not a capability; the calls
        // the delegate made are recorded as steps of their own.
        if name.starts_with(DELEGATE_PREFIX) {
            continue;
        }
        let ok = match step.state {
            StepState::Succeeded => true,
            StepState::Failed => false,
            _ => continue,
        };
        tape.push(Recording {
            capability: name.to_string(),
            args: step.args.clone(),
            ok,
            result,
            used: false,
        });
    }
    Case {
        task: task.id,
        intent: task.intent.clone(),
        outcome: task.state,
        steps: task.steps.len(),
        tokens: task.usage.tokens(),
        tape,
        answers,
    }
}

/// The past tasks `pick` names that can be replayed, oldest first, and a
/// count of the ones left out. `offers` says whether the replay can offer a
/// capability by that name.
pub fn cases(tasks: &[Task], offers: &dyn Fn(&str) -> bool, pick: &Pick) -> (Vec<Case>, Skipped) {
    let mut skipped = Skipped::default();
    let mut chosen = Vec::new();
    match pick {
        Pick::Latest(limit) => {
            let mut latest: Vec<&Task> = tasks.iter().collect();
            latest.sort_by_key(|t| std::cmp::Reverse(t.id));
            for task in latest {
                if chosen.len() == *limit {
                    break;
                }
                if !unsuitable(task, offers, &mut skipped) {
                    chosen.push(case(task));
                }
            }
            chosen.reverse();
        }
        Pick::Tasks(ids) => {
            for id in ids {
                match tasks.iter().find(|task| task.id == *id) {
                    None => skipped.not_found += 1,
                    Some(task) => {
                        if !unsuitable(task, offers, &mut skipped) {
                            chosen.push(case(task));
                        }
                    }
                }
            }
        }
    }
    (chosen, skipped)
}

/// The recording of one case, shared by its stand-ins.
#[derive(Default)]
struct Tape {
    recordings: Vec<Recording>,
    /// Calls the recording did not hold, which were therefore not made.
    missing: Vec<String>,
}

fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max).collect();
    cut.push('…');
    cut
}

/// A recorded failure as the error it was. The butler stored the error's
/// display text, so the prefix it added comes off again; otherwise the
/// planner would see it twice.
fn recorded_error(result: &Value) -> CapabilityError {
    let text = result
        .as_str()
        .map_or_else(|| result.to_string(), str::to_string);
    if let Some(rest) = text.strip_prefix("参数无效：") {
        CapabilityError::InvalidArgs(rest.to_string())
    } else if let Some(rest) = text.strip_prefix("执行失败：") {
        CapabilityError::Failed(rest.to_string())
    } else {
        CapabilityError::Failed(text)
    }
}

impl Tape {
    /// The next unused recording of exactly this call. The same call made
    /// twice gets the results it got the first and second time.
    fn play(&mut self, capability: &str, args: &Value) -> Result<Value, CapabilityError> {
        let found = self
            .recordings
            .iter_mut()
            .find(|r| !r.used && r.capability == capability && r.args == *args);
        match found {
            Some(recording) => {
                recording.used = true;
                if recording.ok {
                    Ok(recording.result.clone())
                } else {
                    Err(recorded_error(&recording.result))
                }
            }
            None => {
                self.missing
                    .push(clip(&format!("{capability} {args}"), MAX_CALL_CHARS));
                Err(CapabilityError::Failed(
                    "回放：录像里没有这次调用，没有执行".into(),
                ))
            }
        }
    }
}

fn lock(tape: &Mutex<Tape>) -> MutexGuard<'_, Tape> {
    tape.lock().unwrap_or_else(|p| p.into_inner())
}

/// A capability as the planner and the policy engine know it, answering from
/// the recording. The real capability is kept only for what it says about
/// itself; it is never invoked.
struct Recorded {
    real: Arc<dyn Capability>,
    tape: Arc<Mutex<Tape>>,
}

#[async_trait]
impl Capability for Recorded {
    fn spec(&self) -> &CapabilitySpec {
        self.real.spec()
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        lock(&self.tape).play(self.real.spec().name.as_str(), &args)
    }

    // The same taint and the same scheduling as the real one, so the replay
    // plans and confirms exactly as the system would.
    fn untrusted_output(&self) -> bool {
        self.real.untrusted_output()
    }

    fn abortable(&self) -> bool {
        self.real.abortable()
    }
}

/// One planner for every case's butler.
struct Shared(Arc<dyn Planner>);

#[async_trait]
impl Planner for Shared {
    async fn plan(&self, intent: &Intent, available: &[CapabilitySpec]) -> Plan {
        self.0.plan(intent, available).await
    }

    async fn plan_turn(&self, conv: &Conversation<'_>) -> Plan {
        self.0.plan_turn(conv).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Ended the same way, without leaving the recording.
    Same,
    /// Did not succeed then, succeeds now.
    Improved,
    /// Succeeded then, does not now.
    Regressed,
    /// Ended differently, neither a success gained nor lost (unhandled
    /// instead of failed, say).
    Changed,
    /// Made a call or asked a question the recording does not hold. What
    /// would have happened is unknown, so the outcome is not compared:
    /// counting it as a success or a failure would be a guess.
    Diverged,
}

pub fn verdict(recorded: TaskState, replayed: TaskState, diverged: bool) -> Verdict {
    if diverged {
        Verdict::Diverged
    } else if recorded == replayed {
        Verdict::Same
    } else if replayed == TaskState::Succeeded {
        Verdict::Improved
    } else if recorded == TaskState::Succeeded {
        Verdict::Regressed
    } else {
        Verdict::Changed
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CaseResult {
    pub task: String,
    pub intent: String,
    pub recorded: TaskState,
    pub replayed: TaskState,
    pub verdict: Verdict,
    pub recorded_steps: usize,
    pub replayed_steps: usize,
    pub recorded_tokens: u64,
    pub replayed_tokens: u64,
    /// Calls the recording did not hold, and so were not made.
    pub missing: Vec<String>,
    /// The replay asked the user something the recording has no answer to.
    pub unanswered: bool,
    /// What the replayed task said about how it ended.
    pub message: Option<String>,
}

/// Runs cases against the capabilities, agents, budget and planner the
/// system has now.
pub struct Replayer {
    capabilities: Vec<Arc<dyn Capability>>,
    agents: AgentDirectory,
    budget: Budget,
    planner: Arc<dyn Planner>,
    scratch: PathBuf,
    learned: Learned,
    /// One throwaway vault for every case: deriving a PIN hash is slow on
    /// purpose, and a replay may run hundreds of cases.
    pins: OnceLock<Arc<PinVault>>,
}

impl Replayer {
    /// `scratch` holds each case's throwaway audit log, and one PIN vault for
    /// all of them, while it runs; it must not be a directory anything else
    /// uses.
    pub fn new(
        capabilities: Vec<Arc<dyn Capability>>,
        agents: AgentDirectory,
        budget: Budget,
        planner: Arc<dyn Planner>,
        scratch: PathBuf,
    ) -> Self {
        Self {
            capabilities,
            agents,
            budget,
            planner,
            scratch,
            learned: Learned::default(),
            pins: OnceLock::new(),
        }
    }

    fn pins(&self) -> io::Result<Arc<PinVault>> {
        if let Some(pins) = self.pins.get() {
            return Ok(pins.clone());
        }
        agentos_runtime::create_private_dir(&self.scratch)?;
        let pins = Arc::new(PinVault::open(self.scratch.join("pin.json"))?);
        pins.set(REPLAY_PIN)?;
        Ok(self.pins.get_or_init(|| pins).clone())
    }

    /// Replay with these lessons and skills in effect, as the system has
    /// them now.
    pub fn with_learned(mut self, learned: Learned) -> Self {
        self.learned = learned;
        self
    }

    /// Whether a recorded call to `name` could be answered in a replay.
    pub fn offers(&self, name: &str) -> bool {
        let learned_skill = |id: &str| self.learned.skills().iter().any(|m| m.id == id);
        name == ASK_USER
            || self
                .capabilities
                .iter()
                .any(|capability| capability.spec().name.as_str() == name)
            || name
                .strip_prefix(DELEGATE_PREFIX)
                .is_some_and(|id| self.agents.get(id).is_some() || learned_skill(id))
    }

    pub async fn run(&self, case: &Case) -> io::Result<CaseResult> {
        self.run_with(case, &self.learned).await
    }

    /// Replay with `learned` in effect instead: how a candidate lesson or
    /// skill is tried without adopting it.
    pub async fn run_with(&self, case: &Case, learned: &Learned) -> io::Result<CaseResult> {
        static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let run = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = self.scratch.join(format!("case-{}-{run}", case.task.0));
        agentos_runtime::create_private_dir(&dir)?;
        let result = self.run_in(case, &dir, learned).await;
        // The scratch log holds the replayed conversation's calls; it is not
        // a record of anything that happened and must not linger.
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    async fn run_in(
        &self,
        case: &Case,
        dir: &std::path::Path,
        learned: &Learned,
    ) -> io::Result<CaseResult> {
        let tape = Arc::new(Mutex::new(Tape {
            recordings: case.tape.clone(),
            missing: Vec::new(),
        }));
        let mut registry = CapabilityRegistry::new();
        for real in &self.capabilities {
            registry.register(Arc::new(Recorded {
                real: real.clone(),
                tape: tape.clone(),
            }));
        }
        let butler_id = AgentId::new("butler");
        let mut policy = PolicyEngine::new();
        let names: Vec<CapabilityName> = registry.specs().into_iter().map(|s| s.name).collect();
        grant_manifest(&mut policy, &butler_id, &names);
        let audit = Arc::new(AuditLog::open(dir.join("audit.jsonl"))?);
        // Shared, so a grant for this case's task must not outlive it: it is
        // made right before the approval that uses it, and runs are one at
        // a time.
        let pins = self.pins()?;
        let butler = Butler::new(
            butler_id,
            registry,
            policy,
            Box::new(Shared(self.planner.clone())),
            audit.clone(),
            ExpressionHub::new(audit),
        )
        .with_agents(self.agents.clone())
        .with_learned(Arc::new(LearnedStore::memory(learned.clone())))
        .with_pin_vault(pins.clone())
        .with_budget(self.budget);

        let id = butler.tasks().create(case.intent.clone())?.id;
        butler.execute(id).await;
        let mut answers = case.answers.iter();
        let mut unanswered = false;
        for _ in 0..MAX_PAUSES {
            let Some(task) = butler.tasks().get(id) else {
                break;
            };
            match (task.state, &task.confirmation, &task.question) {
                (TaskState::AwaitingConfirmation, Some(confirmation), _) => {
                    if confirmation.verify_identity {
                        pins.verify_for(Some(id.0), REPLAY_PIN);
                    }
                    butler
                        .approve_with_token(
                            id,
                            confirmation.step,
                            &task.intent,
                            &confirmation.token,
                        )
                        .await;
                }
                (TaskState::AwaitingInput, _, Some(question)) => match answers.next() {
                    Some(answer) => {
                        butler.reply(id, question.step, &task.intent, answer).await;
                    }
                    None => {
                        unanswered = true;
                        butler.tasks().cancel(id)?;
                        break;
                    }
                },
                _ => break,
            }
        }
        // Still paused after every allowed pause: stop it, it has no outcome.
        if butler
            .tasks()
            .state(id)
            .is_some_and(|state| !state.is_terminal())
        {
            butler.tasks().cancel(id)?;
        }
        let task = butler
            .tasks()
            .get(id)
            .ok_or_else(|| io::Error::other("回放的任务不见了"))?;
        let missing = std::mem::take(&mut lock(&tape).missing);
        let diverged = !missing.is_empty() || unanswered;
        Ok(CaseResult {
            task: case.task.to_string(),
            intent: clip(&case.intent.text, MAX_QUOTE_CHARS),
            recorded: case.outcome,
            replayed: task.state,
            verdict: verdict(case.outcome, task.state, diverged),
            recorded_steps: case.steps,
            replayed_steps: task.steps.len(),
            recorded_tokens: case.tokens,
            replayed_tokens: task.usage.tokens(),
            missing,
            unanswered,
            message: task.message.clone(),
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Summary {
    pub cases: Vec<CaseResult>,
    pub skipped: Skipped,
}

impl Summary {
    pub fn count(&self, verdict: Verdict) -> usize {
        self.cases.iter().filter(|c| c.verdict == verdict).count()
    }

    /// A change is not safe to ship while any case that used to succeed no
    /// longer does.
    pub fn regressed(&self) -> bool {
        self.count(Verdict::Regressed) > 0
    }
}

fn state_label(state: TaskState) -> &'static str {
    match state {
        TaskState::Succeeded => "成功",
        TaskState::Unhandled => "无法处理",
        TaskState::Failed => "失败",
        TaskState::Cancelled => "已停止",
        TaskState::Interrupted => "中断",
        _ => "未结束",
    }
}

fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Same => "相同",
        Verdict::Improved => "变好",
        Verdict::Regressed => "退化",
        Verdict::Changed => "改变",
        Verdict::Diverged => "偏离录像",
    }
}

pub fn render(summary: &Summary, planner: &str) -> String {
    use std::fmt::Write;

    let mut out = format!("回放 {} 个任务（{planner}）", summary.cases.len());
    let s = &summary.skipped;
    if s.total() > 0 {
        let reasons: Vec<String> = [
            (s.no_outcome, "没有结局"),
            (s.outside_content, "读过外部内容"),
            (s.caller, "来自外部调用方"),
            (s.in_thread, "接着前面的对话"),
            (s.unavailable, "用到回放里没有的能力"),
            (s.not_found, "找不到"),
        ]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, why)| format!("{why} {n}"))
        .collect();
        let _ = write!(out, "，跳过 {}（{}）", s.total(), reasons.join("，"));
    }
    out.push('。');
    if summary.cases.is_empty() {
        out.push_str("\n没有可以回放的任务。");
        return out;
    }
    for c in &summary.cases {
        let _ = write!(
            out,
            "\n[{}] {} “{}”：{} → {}，步骤 {} → {}",
            verdict_label(c.verdict),
            c.task,
            c.intent,
            state_label(c.recorded),
            state_label(c.replayed),
            c.recorded_steps,
            c.replayed_steps
        );
        if c.recorded_tokens > 0 || c.replayed_tokens > 0 {
            let _ = write!(out, "，token {} → {}", c.recorded_tokens, c.replayed_tokens);
        }
        for call in &c.missing {
            let _ = write!(out, "\n    录像里没有、没有执行：{call}");
        }
        if c.unanswered {
            out.push_str("\n    提出了录像里没有答案的问题");
        }
        if c.verdict == Verdict::Regressed {
            if let Some(message) = &c.message {
                let _ = write!(out, "\n    {}", clip(message, MAX_CALL_CHARS));
            }
        }
    }
    let _ = write!(
        out,
        "\n\n相同 {}，变好 {}，退化 {}，改变 {}，偏离录像 {}。",
        summary.count(Verdict::Same),
        summary.count(Verdict::Improved),
        summary.count(Verdict::Regressed),
        summary.count(Verdict::Changed),
        summary.count(Verdict::Diverged)
    );
    let tokens: u64 = summary.cases.iter().map(|c| c.replayed_tokens).sum();
    if tokens > 0 {
        let _ = write!(out, "\n这次回放用了 {tokens} token。");
    }
    if summary.regressed() {
        out.push_str("\n有任务以前成功、现在不成功了。");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::{DeviceId, IntentId, RiskLevel, Urgency};
    use agentos_planner::{Round, Step};
    use agentos_runtime::{TaskStep, TaskStore};
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    /// A capability that counts the times it really ran. Replays must never
    /// make it count.
    struct Real {
        spec: CapabilitySpec,
        runs: AtomicUsize,
    }

    impl Real {
        fn new(name: &str, risk: RiskLevel) -> Arc<Self> {
            Arc::new(Self {
                spec: CapabilitySpec {
                    name: CapabilityName::new(name),
                    description: String::new(),
                    risk,
                    input_schema: json!({"type": "object"}),
                },
                runs: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl Capability for Real {
        fn spec(&self) -> &CapabilitySpec {
            &self.spec
        }

        async fn invoke(&self, _args: Value) -> Result<Value, CapabilityError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"message": "真的执行了"}))
        }
    }

    type Script = dyn Fn(&[Round]) -> Plan + Send + Sync;

    struct Scripted(Box<Script>);

    #[async_trait]
    impl Planner for Scripted {
        async fn plan(&self, _intent: &Intent, _available: &[CapabilitySpec]) -> Plan {
            (self.0)(&[])
        }

        async fn plan_turn(&self, conv: &Conversation<'_>) -> Plan {
            (self.0)(conv.history)
        }
    }

    fn call(capability: &str, args: Value) -> Plan {
        Plan {
            steps: vec![Step::new(CapabilityName::new(capability), args)],
            ..Plan::default()
        }
    }

    fn answer(text: &str) -> Plan {
        Plan {
            reply: Some(text.into()),
            ..Plan::default()
        }
    }

    /// What the planner was given back in its latest round.
    fn last_result(history: &[Round]) -> String {
        history
            .last()
            .and_then(|round| round.results.last())
            .map(|result| result.content.clone())
            .unwrap_or_default()
    }

    static SCRATCH: AtomicU64 = AtomicU64::new(0);

    fn replayer(
        capabilities: Vec<Arc<dyn Capability>>,
        script: impl Fn(&[Round]) -> Plan + Send + Sync + 'static,
    ) -> Replayer {
        let scratch = std::env::temp_dir().join(format!(
            "agentos-replay-{}-{}",
            std::process::id(),
            SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        Replayer::new(
            capabilities,
            AgentDirectory::new(),
            Budget::default(),
            Arc::new(Scripted(Box::new(script))),
            scratch,
        )
    }

    /// A finished task with recorded steps (capability, args, ok, result).
    fn recorded(
        store: &TaskStore,
        text: &str,
        state: TaskState,
        steps: &[(&str, Value, bool, Value)],
        change: impl FnOnce(&mut Task),
    ) -> Task {
        let created = store
            .create(Intent {
                id: IntentId(0),
                source: DeviceId::new("unix:uid:1000"),
                speaker: None,
                text: text.into(),
                urgency: Urgency::Normal,
                at_ms: 0,
            })
            .unwrap();
        store
            .update(created.id, |t| {
                for (capability, args, ok, result) in steps {
                    let mut step =
                        TaskStep::pending(CapabilityName::new(*capability), args.clone());
                    step.state = if *ok {
                        StepState::Succeeded
                    } else {
                        StepState::Failed
                    };
                    step.result = Some(result.clone());
                    t.steps.push(step);
                }
                t.state = state;
                change(t);
            })
            .unwrap()
    }

    fn only_case(task: &Task, replayer: &Replayer) -> Case {
        let (mut cases, skipped) = cases(
            std::slice::from_ref(task),
            &|name| replayer.offers(name),
            &Pick::Latest(10),
        );
        assert_eq!(skipped.total(), 0, "{skipped:?}");
        cases.remove(0)
    }

    #[tokio::test]
    async fn the_same_plan_against_its_recording_ends_the_same_without_running_anything() {
        let real = Real::new("notes.list", RiskLevel::ReadOnly);
        let r = replayer(vec![real.clone()], |history| match history.len() {
            0 => call("notes.list", json!({})),
            // The planner is handed what was recorded, not a live result.
            _ => answer(&format!("看到了 {}", last_result(history))),
        });
        let store = TaskStore::memory();
        let task = recorded(
            &store,
            "看看笔记",
            TaskState::Succeeded,
            &[(
                "notes.list",
                json!({}),
                true,
                json!({"message": "1. 买牛奶"}),
            )],
            |_| {},
        );
        let result = r.run(&only_case(&task, &r)).await.unwrap();
        assert_eq!(result.verdict, Verdict::Same, "{result:?}");
        assert_eq!(result.replayed, TaskState::Succeeded);
        assert_eq!(result.replayed_steps, 1);
        assert!(result.missing.is_empty());
        // Guards "nothing real happens": not even a read goes through.
        assert_eq!(real.runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_call_the_recording_does_not_hold_is_never_made_and_the_outcome_is_not_guessed() {
        let real = Real::new("notes.append", RiskLevel::Reversible);
        let r = replayer(vec![real.clone()], |history| match history.len() {
            0 => call("notes.append", json!({"text": "别的内容"})),
            _ => answer("记好了"),
        });
        let store = TaskStore::memory();
        let task = recorded(
            &store,
            "记一下 买牛奶",
            TaskState::Succeeded,
            &[(
                "notes.append",
                json!({"text": "买牛奶"}),
                true,
                json!({"message": "已记下"}),
            )],
            |_| {},
        );
        let result = r.run(&only_case(&task, &r)).await.unwrap();
        // Guards the safety rule and "outcomes are told truthfully": the
        // write was not made, and the case is neither a success nor a
        // regression, because nobody knows how it would have ended.
        assert_eq!(real.runs.load(Ordering::SeqCst), 0);
        assert_eq!(result.verdict, Verdict::Diverged);
        assert_eq!(result.missing.len(), 1);
        assert!(
            result.missing[0].contains("别的内容"),
            "{:?}",
            result.missing
        );
        let summary = Summary {
            cases: vec![result],
            skipped: Skipped::default(),
        };
        assert!(!summary.regressed());
        assert!(render(&summary, "测试").contains("录像里没有、没有执行"));
    }

    #[tokio::test]
    async fn confirmations_are_approved_because_only_stand_ins_stand_behind_them() {
        let send = Real::new("mail.send", RiskLevel::External);
        let install = Real::new("plugin.install", RiskLevel::Irreversible);
        let r = replayer(
            vec![send.clone(), install.clone()],
            |history| match history.len() {
                0 => call("mail.send", json!({"to": "a"})),
                1 => call("plugin.install", json!({})),
                _ => answer("发了，也装了"),
            },
        );
        let store = TaskStore::memory();
        let task = recorded(
            &store,
            "发邮件再装插件",
            TaskState::Succeeded,
            &[
                (
                    "mail.send",
                    json!({"to": "a"}),
                    true,
                    json!({"message": "已发送"}),
                ),
                (
                    "plugin.install",
                    json!({}),
                    true,
                    json!({"message": "已安装"}),
                ),
            ],
            |_| {},
        );
        let result = r.run(&only_case(&task, &r)).await.unwrap();
        assert_eq!(result.verdict, Verdict::Same, "{result:?}");
        assert_eq!(send.runs.load(Ordering::SeqCst), 0);
        assert_eq!(install.runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_recorded_failure_is_played_back_as_the_same_failure() {
        let real = Real::new("weather.get", RiskLevel::ReadOnly);
        let r = replayer(vec![real.clone()], |history| match history.len() {
            0 => call("weather.get", json!({"city": "杭州"})),
            _ => answer(&last_result(history)),
        });
        let store = TaskStore::memory();
        let task = recorded(
            &store,
            "杭州天气",
            TaskState::Failed,
            &[(
                "weather.get",
                json!({"city": "杭州"}),
                false,
                json!("执行失败：连接超时"),
            )],
            |_| {},
        );
        let result = r.run(&only_case(&task, &r)).await.unwrap();
        assert_eq!(result.verdict, Verdict::Same, "{result:?}");
        assert_eq!(result.replayed, TaskState::Failed);
        // The error reads as it did, not with the prefix added twice.
        let error = recorded_error(&json!("执行失败：连接超时")).to_string();
        assert_eq!(error, "执行失败：连接超时");
        let error = recorded_error(&json!("参数无效：缺少 city")).to_string();
        assert_eq!(error, "参数无效：缺少 city");
    }

    #[tokio::test]
    async fn a_question_is_answered_with_what_the_user_said_then() {
        let r = replayer(Vec::new(), |history| match history.len() {
            0 => call(ASK_USER, json!({"question": "去哪个城市？"})),
            _ => answer(&format!("好的，{}", last_result(history))),
        });
        let store = TaskStore::memory();
        let task = recorded(
            &store,
            "订酒店",
            TaskState::Succeeded,
            &[(
                ASK_USER,
                json!({"question": "去哪个城市？"}),
                true,
                json!("杭州"),
            )],
            |_| {},
        );
        let result = r.run(&only_case(&task, &r)).await.unwrap();
        assert_eq!(result.verdict, Verdict::Same, "{result:?}");

        // A question the recording has no answer to stops the case: making
        // one up would be speaking for the user.
        let silent = recorded(&store, "订机票", TaskState::Succeeded, &[], |_| {});
        let result = r.run(&only_case(&silent, &r)).await.unwrap();
        assert!(result.unanswered);
        assert_eq!(result.verdict, Verdict::Diverged);
    }

    #[test]
    fn regressions_and_improvements_are_told_apart() {
        use TaskState::*;
        assert_eq!(verdict(Succeeded, Succeeded, false), Verdict::Same);
        assert_eq!(verdict(Failed, Succeeded, false), Verdict::Improved);
        assert_eq!(verdict(Unhandled, Succeeded, false), Verdict::Improved);
        assert_eq!(verdict(Succeeded, Failed, false), Verdict::Regressed);
        assert_eq!(verdict(Succeeded, Unhandled, false), Verdict::Regressed);
        assert_eq!(verdict(Failed, Unhandled, false), Verdict::Changed);
        // Off the recording, even an apparent success is not counted.
        assert_eq!(verdict(Failed, Succeeded, true), Verdict::Diverged);
        assert_eq!(verdict(Succeeded, Failed, true), Verdict::Diverged);
    }

    #[test]
    fn only_tasks_that_can_be_compared_fairly_are_replayed() {
        let store = TaskStore::memory();
        let ok = |text: &str| recorded(&store, text, TaskState::Succeeded, &[], |_| {});
        let first = ok("几点了");
        let tasks = vec![
            first.clone(),
            recorded(&store, "取消了", TaskState::Cancelled, &[], |_| {}),
            recorded(&store, "总结网页", TaskState::Succeeded, &[], |t| {
                t.tainted = true
            }),
            recorded(&store, "调用方", TaskState::Succeeded, &[], |t| {
                t.intent.source = DeviceId::new("caller:helper")
            }),
            {
                // The store keeps a thread fixed; a task continuing one is
                // made here as it would be read back.
                let mut task = ok("接着说");
                task.thread = first.id;
                task
            },
            recorded(
                &store,
                "开灯",
                TaskState::Succeeded,
                &[("home.turn_on", json!({}), true, json!({}))],
                |_| {},
            ),
            ok("最新的"),
        ];
        let offers = |name: &str| name != "home.turn_on";
        let (chosen, skipped) = cases(&tasks, &offers, &Pick::Latest(10));
        let texts: Vec<&str> = chosen.iter().map(|c| c.intent.text.as_str()).collect();
        assert_eq!(texts, ["几点了", "最新的"]);
        assert_eq!(
            skipped,
            Skipped {
                no_outcome: 1,
                outside_content: 1,
                caller: 1,
                in_thread: 1,
                unavailable: 1,
                not_found: 0,
            }
        );
        // The latest first when there are more than asked for, still
        // replayed oldest first.
        let (chosen, _) = cases(&tasks, &offers, &Pick::Latest(1));
        assert_eq!(chosen[0].intent.text, "最新的");
        let (chosen, skipped) = cases(&tasks, &offers, &Pick::Tasks(vec![first.id, TaskId(999)]));
        assert_eq!(chosen.len(), 1);
        assert_eq!(skipped.not_found, 1);
    }
}
