//! The self-improvement cycle: learn a lesson or a skill from the user's own
//! history, replay past tasks with and without it, and adopt it only when the
//! replay shows it clearly better. Adopted changes are watched afterwards and
//! withdrawn automatically if the tasks after them do worse.
//!
//! Stage 3 of docs/self-improvement-research.md, section 4.1. Adoption needs
//! no approval by design: what is adopted (lessons, skills) cannot widen
//! what the system may do, the bar is set and checked here where the
//! generator cannot reach it, and every adoption is audited and can be
//! undone with `improve.rollback`.
//!
//! What the generator is shown comes only from tasks that speak for the
//! user (not tainted, not from a caller). Outside content written into a
//! lesson would steer every later task, so it never gets that far.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use agentos_audit::{AuditEvent, AuditLog};
use agentos_core::{now_ms, Capability, CapabilitySpec, RiskLevel};
use agentos_planner::llm::TextModel;
use agentos_planner::Planner;
use agentos_runtime::{StepState, Task, TaskId, TaskState, TaskStore};
use agentos_services::agents::{AgentDirectory, DELEGATE_PREFIX};
use serde::Serialize;
use serde_json::{json, Value};

use crate::butler::{Budget, Butler, ASK_USER};
use crate::learned::{Attempt, Content, Item, Learned, LearnedStore, Rate, Status};
use crate::replay::{self, Case, CaseResult, Pick, Replayer, Verdict};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// What a replay needs from the running system: its capabilities and its
/// configured agents, as they are at the moment of the cycle.
pub trait System: Send + Sync {
    fn capabilities(&self) -> Vec<Arc<dyn Capability>>;
    fn agents(&self) -> AgentDirectory;
}

impl System for Butler {
    fn capabilities(&self) -> Vec<Arc<dyn Capability>> {
        self.capability_handles()
    }

    fn agents(&self) -> AgentDirectory {
        self.configured_agents()
    }
}

/// When a candidate counts as clearly better. The numbers are the ones in
/// the research doc; they live here, out of the generator's reach.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Gate {
    /// Cases compared, at least.
    pub min_cases: usize,
    /// Times each case is replayed, with and without the candidate.
    pub runs: usize,
    /// Cases that must turn into successes in most runs.
    pub min_improved: usize,
    /// Or, with every outcome the same: candidate tokens or steps at most
    /// this share of the baseline's.
    pub efficiency: f64,
    /// Cases the candidate may leave the recording on beyond the baseline.
    pub max_extra_diverged: usize,
    /// Candidate tokens at most this multiple of the baseline's.
    pub max_cost: f64,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            min_cases: 10,
            runs: 3,
            min_improved: 2,
            efficiency: 0.8,
            max_extra_diverged: 1,
            max_cost: 1.2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Evaluation {
    pub cases: usize,
    pub runs: usize,
    /// Cases that became successes with the candidate in most runs, and not
    /// without it.
    pub improved: Vec<String>,
    /// Cases that, in any run with the candidate, no longer succeeded.
    pub regressed: Vec<String>,
    pub diverged_baseline: usize,
    pub diverged_candidate: usize,
    pub baseline_tokens: u64,
    pub candidate_tokens: u64,
    pub baseline_steps: usize,
    pub candidate_steps: usize,
    pub passed: bool,
    pub reason: String,
}

fn majority(count: usize, runs: usize) -> bool {
    count * 2 > runs
}

/// Weigh replays with and without the candidate: for each case, its runs
/// without (`.0`) and with (`.1`) it.
pub fn judge(gate: &Gate, results: &[(Vec<CaseResult>, Vec<CaseResult>)]) -> Evaluation {
    let runs = gate.runs.max(1);
    let mut e = Evaluation {
        cases: results.len(),
        runs,
        improved: Vec::new(),
        regressed: Vec::new(),
        diverged_baseline: 0,
        diverged_candidate: 0,
        baseline_tokens: 0,
        candidate_tokens: 0,
        baseline_steps: 0,
        candidate_steps: 0,
        passed: false,
        reason: String::new(),
    };
    let count = |runs: &[CaseResult], v: Verdict| runs.iter().filter(|r| r.verdict == v).count();
    let mut all_same = true;
    let (mut comparable_base, mut comparable_cand) = ((0u64, 0usize), (0u64, 0usize));
    for (base, cand) in results {
        let Some(name) = base.first().or(cand.first()).map(|r| r.task.clone()) else {
            continue;
        };
        e.baseline_tokens += base.iter().map(|r| r.replayed_tokens).sum::<u64>();
        e.candidate_tokens += cand.iter().map(|r| r.replayed_tokens).sum::<u64>();
        e.baseline_steps += base.iter().map(|r| r.replayed_steps).sum::<usize>();
        e.candidate_steps += cand.iter().map(|r| r.replayed_steps).sum::<usize>();
        if count(cand, Verdict::Regressed) > 0 {
            e.regressed.push(name.clone());
        }
        let base_diverged = majority(count(base, Verdict::Diverged), runs);
        let cand_diverged = majority(count(cand, Verdict::Diverged), runs);
        e.diverged_baseline += usize::from(base_diverged);
        e.diverged_candidate += usize::from(cand_diverged);
        if majority(count(cand, Verdict::Improved), runs)
            && !majority(count(base, Verdict::Improved), runs)
        {
            e.improved.push(name);
        }
        if !base_diverged && !cand_diverged {
            all_same &= cand.iter().all(|r| r.verdict == Verdict::Same);
            comparable_base.0 += base.iter().map(|r| r.replayed_tokens).sum::<u64>();
            comparable_base.1 += base.iter().map(|r| r.replayed_steps).sum::<usize>();
            comparable_cand.0 += cand.iter().map(|r| r.replayed_tokens).sum::<u64>();
            comparable_cand.1 += cand.iter().map(|r| r.replayed_steps).sum::<usize>();
        }
    }
    let leaner = |base: f64, cand: f64| base > 0.0 && cand <= base * gate.efficiency;
    e.reason = if e.cases < gate.min_cases {
        format!("可比较的用例只有 {} 个，至少要 {}", e.cases, gate.min_cases)
    } else if !e.regressed.is_empty() {
        format!("{} 退化了", e.regressed.join("、"))
    } else if e.diverged_candidate > e.diverged_baseline + gate.max_extra_diverged {
        format!(
            "偏离录像的用例从 {} 个变成 {} 个，无法判断",
            e.diverged_baseline, e.diverged_candidate
        )
    } else if e.baseline_tokens > 0
        && e.candidate_tokens as f64 > e.baseline_tokens as f64 * gate.max_cost
    {
        format!("token 从 {} 涨到 {}", e.baseline_tokens, e.candidate_tokens)
    } else if e.improved.len() >= gate.min_improved {
        e.passed = true;
        format!(
            "{} 个用例稳定变好：{}",
            e.improved.len(),
            e.improved.join("、")
        )
    } else if all_same
        && (leaner(comparable_base.0 as f64, comparable_cand.0 as f64)
            || leaner(comparable_base.1 as f64, comparable_cand.1 as f64))
    {
        e.passed = true;
        format!(
            "结局都相同，token {} → {}，步骤 {} → {}",
            comparable_base.0, comparable_cand.0, comparable_base.1, comparable_cand.1
        )
    } else {
        "没有明显变好".into()
    };
    e
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Adopt what passes. Off, a passing candidate is only reported.
    pub auto: bool,
    /// Tokens one cycle may spend on generating and replaying.
    pub tokens: u64,
    pub gate: Gate,
    /// Recent tasks replayed besides the ones a candidate came from.
    pub cases: usize,
    /// A task that led nowhere is not tried again for this long.
    pub retry_after_ms: u64,
    /// The watch after adopting: this many of the user's tasks...
    pub observe_tasks: usize,
    /// ...or this long, whichever comes first with no sign of harm.
    pub observe_ms: u64,
    /// Withdraw when the success rate drops by more than this.
    pub max_drop: f64,
    /// Adoptions per day, at most.
    pub per_day: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto: true,
            tokens: 2_000_000,
            gate: Gate::default(),
            cases: 20,
            retry_after_ms: 7 * DAY_MS,
            observe_tasks: 20,
            observe_ms: 3 * DAY_MS,
            max_drop: 0.10,
            per_day: 3,
        }
    }
}

impl Config {
    /// `AGENTOS_IMPROVE_AUTO=0` reports instead of adopting;
    /// `AGENTOS_IMPROVE_TOKENS` caps one cycle's spending (`0` is no cap).
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut config = Self::default();
        if let Some(value) = var("AGENTOS_IMPROVE_AUTO") {
            config.auto = match value.trim() {
                "1" | "on" | "" => true,
                "0" | "off" => false,
                other => return Err(format!("AGENTOS_IMPROVE_AUTO 只能是 1 或 0，不是 {other}")),
            };
        }
        if let Some(value) = var("AGENTOS_IMPROVE_TOKENS") {
            config.tokens = match value.trim().parse::<u64>() {
                Ok(0) => u64::MAX,
                Ok(n) => n,
                Err(_) => return Err(format!("AGENTOS_IMPROVE_TOKENS 必须是整数，不是 {value}")),
            };
        }
        Ok(config)
    }
}

/// How one cycle went, for the log.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Applied {
        id: String,
        summary: String,
    },
    Withheld {
        summary: String,
        reason: String,
    },
    Rejected {
        summary: String,
        reason: String,
    },
    RolledBack {
        id: String,
        reason: String,
    },
    Settled {
        id: String,
    },
    /// Nothing to do this time, and why.
    Idle(String),
    /// The user came back; the cycle stopped where it was.
    Yielded,
}

impl Outcome {
    pub fn describe(&self) -> String {
        match self {
            Outcome::Applied { id, summary } => format!("自我改进：采用了 {id}（{summary}）"),
            Outcome::Withheld { summary, reason } => {
                format!("自我改进：{summary} 通过了评测（{reason}），但自动生效已关闭")
            }
            Outcome::Rejected { summary, reason } => {
                format!("自我改进：没有采用 {summary}：{reason}")
            }
            Outcome::RolledBack { id, reason } => format!("自我改进：撤回了 {id}：{reason}"),
            Outcome::Settled { id } => format!("自我改进：{id} 观察期结束，保留"),
            Outcome::Idle(why) => format!("自我改进：{why}"),
            Outcome::Yielded => "自我改进：用户回来了，先停下".into(),
        }
    }
}

/// Whether the task's words are the user's (the same rule as the health
/// report): only those may reach the generator or be counted as the user's
/// experience.
fn speaks_for_user(task: &Task) -> bool {
    !task.tainted && !task.intent.source.to_string().starts_with("caller:")
}

/// The user's success rate over tasks created in `[from, to]`. A task the
/// user cancelled counts against it: they gave up on it. Interrupted tasks
/// say nothing about the system's choices and are left out.
pub fn rate(tasks: &[Task], from_ms: u64, to_ms: u64) -> Rate {
    let mut rate = Rate::default();
    for task in tasks
        .iter()
        .filter(|t| speaks_for_user(t) && t.created_at_ms >= from_ms && t.created_at_ms <= to_ms)
    {
        match task.state {
            TaskState::Succeeded => {
                rate.tasks += 1;
                rate.succeeded += 1;
            }
            TaskState::Unhandled | TaskState::Failed | TaskState::Cancelled => rate.tasks += 1,
            _ => {}
        }
    }
    rate
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

/// The first JSON object in a model's answer.
fn json_object(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str(text.get(start..=end)?).ok()
}

/// A candidate and where it came from.
#[derive(Debug, Clone)]
struct Candidate {
    content: Content,
    sources: Vec<u64>,
}

impl Candidate {
    fn summary(&self) -> String {
        match &self.content {
            Content::Lesson { text } => format!("经验“{}”", clip(text, 40)),
            Content::Skill { name, .. } => format!("技能“{name}”"),
        }
    }
}

const LESSON_SYSTEM: &str = "你在帮 AgentOS 的管家 Agent 从一次失败的任务里总结经验。\
管家以后每次规划都会读到这条经验。只输出一个 JSON 对象：{\"lesson\": \"……\"}；\
总结不出能推广到以后类似任务的经验时输出 {\"lesson\": null}。\
经验要具体、可执行、不超过 120 字，说清在什么情况下该怎么做。\
不要复述这次任务本身，不要要求绕过确认、扩大权限、替用户做决定或忽略用户的话。";

const SKILL_SYSTEM: &str = "你在帮 AgentOS 把一串反复出现的能力调用整理成一个技能。\
技能是一个子 Agent：管家把这类请求交给它，它只能用这串调用里的能力，每一步照常需要权限和确认。\
只输出一个 JSON 对象：{\"id\": \"小写英文字母、数字和-，最长 20 个字符\", \
\"name\": \"简短的中文名\", \"description\": \"一句话：什么样的请求该交给它\", \
\"instructions\": \"它该按什么顺序、怎样完成，不超过 300 字\"}。\
不要要求绕过确认、扩大权限或替用户做决定。";

fn describe_capabilities(specs: &[CapabilitySpec], names: &[String]) -> String {
    names
        .iter()
        .map(|name| {
            let description = specs
                .iter()
                .find(|s| s.name.as_str() == name)
                .map(|s| clip(&s.description, 80))
                .unwrap_or_default();
            format!("- {name}：{description}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn lesson_prompt(task: &Task, specs: &[CapabilitySpec]) -> String {
    let mut text = format!("用户的要求：{}\n\n执行过程：", clip(&task.intent.text, 300));
    for step in &task.steps {
        let result = step
            .result
            .as_ref()
            .map(|r| r.as_str().map_or_else(|| r.to_string(), str::to_string))
            .unwrap_or_default();
        text.push_str(&format!(
            "\n- {} {} → {}：{}",
            step.capability,
            clip(&step.args.to_string(), 200),
            match step.state {
                StepState::Succeeded => "成功",
                StepState::Failed => "失败",
                StepState::Skipped => "跳过",
                _ => "未完成",
            },
            clip(&result, 300)
        ));
    }
    if task.steps.is_empty() {
        text.push_str("（没有调用任何能力）");
    }
    text.push_str(&format!(
        "\n\n结局：失败。{}",
        clip(task.message.as_deref().unwrap_or_default(), 300)
    ));
    let names: Vec<String> = specs.iter().map(|s| s.name.to_string()).collect();
    text.push_str("\n\n管家能用的能力：\n");
    text.push_str(&describe_capabilities(specs, &names));
    text
}

/// Successful tasks of the user's that ran the same sequence of calls, by
/// sequence, most frequent first. Only sequences of two or more calls that a
/// skill may use.
fn recurring(tasks: &[&Task], specs: &[CapabilitySpec]) -> Vec<(Vec<String>, Vec<u64>)> {
    let mut groups: BTreeMap<Vec<String>, Vec<u64>> = BTreeMap::new();
    for task in tasks
        .iter()
        .filter(|t| t.state == TaskState::Succeeded && speaks_for_user(t))
    {
        let mut sequence: Vec<String> = Vec::new();
        for step in &task.steps {
            let name = step.capability.to_string();
            if sequence.last() != Some(&name) {
                sequence.push(name);
            }
        }
        let usable = sequence.len() >= 2
            && sequence.iter().all(|name| {
                name != ASK_USER
                    && !name.starts_with(DELEGATE_PREFIX)
                    && specs.iter().any(|s| s.name.as_str() == name)
            });
        if usable {
            groups.entry(sequence).or_default().push(task.id.0);
        }
    }
    let mut groups: Vec<(Vec<String>, Vec<u64>)> = groups
        .into_iter()
        .filter(|(_, ids)| ids.len() >= 3)
        .collect();
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
    groups
}

fn skill_prompt(sequence: &[String], examples: &[&Task], specs: &[CapabilitySpec]) -> String {
    let mut text = format!(
        "这串调用在 {} 个任务里反复出现：\n{}\n\n例如用户说过：",
        examples.len(),
        describe_capabilities(specs, sequence)
    );
    for task in examples.iter().take(5) {
        text.push_str(&format!("\n- {}", clip(&task.intent.text, 120)));
    }
    text
}

fn slug(text: &str) -> String {
    let s: String = text
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').chars().take(20).collect::<String>();
    s.trim_end_matches('-').to_string()
}

pub struct Improver {
    tasks: Arc<TaskStore>,
    audit: Arc<AuditLog>,
    learned: Arc<LearnedStore>,
    system: Arc<dyn System>,
    model: Arc<dyn TextModel>,
    planner: Arc<dyn Planner>,
    /// Which model the planner is; recorded with what it proved.
    planner_label: String,
    budget: Budget,
    config: Config,
    scratch: PathBuf,
}

impl Improver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tasks: Arc<TaskStore>,
        audit: Arc<AuditLog>,
        learned: Arc<LearnedStore>,
        system: Arc<dyn System>,
        model: Arc<dyn TextModel>,
        planner: Arc<dyn Planner>,
        planner_label: String,
        budget: Budget,
        config: Config,
        scratch: PathBuf,
    ) -> Self {
        Self {
            tasks,
            audit,
            learned,
            system,
            model,
            planner,
            planner_label,
            budget,
            config,
            scratch,
        }
    }

    /// The user is using the system: anything of theirs queued or under way.
    fn user_is_back(&self) -> bool {
        !self.tasks.unfinished().is_empty()
    }

    /// One cycle: first the watch over what was adopted, then at most one
    /// new candidate.
    pub async fn cycle(&self) -> io::Result<Outcome> {
        let now = now_ms();
        if let Some(outcome) = self.observe(now)? {
            return Ok(outcome);
        }
        let (_, learned) = self.learned.current();
        if learned.observing().is_some() {
            // One at a time: while one is watched, a second would make it
            // impossible to tell which one did what.
            return Ok(Outcome::Idle("上一个改进还在观察期".into()));
        }
        let today = learned
            .items
            .iter()
            .filter(|i| i.applied_at_ms + DAY_MS > now)
            .count();
        if today >= self.config.per_day {
            return Ok(Outcome::Idle(format!("今天已经采用了 {today} 个改进")));
        }
        let mut spent = 0u64;
        let Some(candidate) = self.propose(&learned, now, &mut spent).await? else {
            return Ok(Outcome::Idle("没有新的可以学的东西".into()));
        };
        let summary = candidate.summary();
        if let Err(reason) = candidate.content.check() {
            return self.reject(candidate, now, reason);
        }
        let mut with = (*learned).clone();
        if let Err(reason) = with.adopt(
            candidate.content.clone(),
            self.item(&candidate, now, json!({})),
        ) {
            return self.reject(candidate, now, reason);
        }
        let evaluation = match self
            .evaluate(&learned, &with, &candidate, &mut spent)
            .await?
        {
            Ok(evaluation) => evaluation,
            Err(Stop::Yielded) => return Ok(Outcome::Yielded),
            Err(Stop::Reason(reason)) => return self.reject(candidate, now, reason),
        };
        if !evaluation.passed {
            let reason = evaluation.reason.clone();
            return self.reject(candidate, now, reason);
        }
        if !self.config.auto {
            let reason = evaluation.reason.clone();
            self.remember(
                &candidate,
                now,
                format!("通过了评测但自动生效已关闭：{reason}"),
            )?;
            return Ok(Outcome::Withheld { summary, reason });
        }
        let evidence = json!({
            "evaluation": evaluation,
            "model": self.planner_label,
            "tokens_spent": spent,
            "gate": self.config.gate,
        });
        let item = self.item(&candidate, now, evidence.clone());
        let content = candidate.content.clone();
        let adopted = self
            .learned
            .modify(|learned| learned.adopt(content.clone(), item))?;
        let id = match adopted {
            Ok(id) => id,
            Err(reason) => return self.reject(candidate, now, reason),
        };
        // Written once the change is durable; if this fails the item is in
        // effect without its record, so it is withdrawn again.
        if let Err(error) = self.audit.append(AuditEvent::ImprovementApplied {
            id: id.clone(),
            kind: candidate.content.kind().into(),
            content: serde_json::to_value(&candidate.content).unwrap_or(Value::Null),
            evidence,
        }) {
            let _ = self
                .learned
                .modify(|l| l.roll_back(&id, "审计日志写不进去", now_ms()).map(|_| ()));
            return Err(error);
        }
        Ok(Outcome::Applied { id, summary })
    }

    fn item(&self, candidate: &Candidate, now: u64, evidence: Value) -> Item {
        let tasks = self.tasks.list();
        Item {
            id: String::new(),
            content: candidate.content.clone(),
            status: Status::Observing,
            sources: candidate.sources.clone(),
            model: Some(self.planner_label.clone()),
            evidence,
            applied_at_ms: now,
            baseline: rate(&tasks, now.saturating_sub(30 * DAY_MS), now),
            settled_at_ms: None,
            rolled_back_at_ms: None,
            reason: None,
        }
    }

    fn remember(&self, candidate: &Candidate, now: u64, reason: String) -> io::Result<()> {
        self.learned
            .modify(|learned| {
                learned.remember_attempt(Attempt {
                    at_ms: now,
                    sources: candidate.sources.clone(),
                    reason,
                });
                Ok(())
            })?
            .map_err(io::Error::other)
    }

    fn reject(&self, candidate: Candidate, now: u64, reason: String) -> io::Result<Outcome> {
        self.remember(&candidate, now, reason.clone())?;
        Ok(Outcome::Rejected {
            summary: candidate.summary(),
            reason,
        })
    }

    /// The watch over the improvement adopted last: withdraw it when the
    /// user's tasks since do clearly worse than before, keep it once enough
    /// of them (or enough time) passed without that.
    fn observe(&self, now: u64) -> io::Result<Option<Outcome>> {
        let (_, learned) = self.learned.current();
        let Some(item) = learned.observing().cloned() else {
            return Ok(None);
        };
        let tasks = self.tasks.list();
        let after = rate(&tasks, item.applied_at_ms, now);
        let worse = match (item.baseline.value(), after.value()) {
            (Some(before), Some(since)) if after.tasks >= self.config.observe_tasks => {
                (since < before - self.config.max_drop).then_some((before, since))
            }
            _ => None,
        };
        if let Some((before, since)) = worse {
            let reason = format!(
                "采用后 {} 个任务的成功率 {:.0}%，采用前是 {:.0}%",
                after.tasks,
                since * 100.0,
                before * 100.0
            );
            let id = item.id.clone();
            if let Err(refusal) = self
                .learned
                .modify(|l| l.roll_back(&id, &reason, now).map(|_| ()))?
            {
                return Err(io::Error::other(refusal));
            }
            self.audit.append(AuditEvent::ImprovementRolledBack {
                id: id.clone(),
                by: "system".into(),
                reason: reason.clone(),
            })?;
            return Ok(Some(Outcome::RolledBack { id, reason }));
        }
        if after.tasks >= self.config.observe_tasks
            || now >= item.applied_at_ms + self.config.observe_ms
        {
            let id = item.id.clone();
            self.learned
                .modify(|l| {
                    if let Some(i) = l.items.iter_mut().find(|i| i.id == id) {
                        if i.status == Status::Observing {
                            i.status = Status::Active;
                            i.settled_at_ms = Some(now);
                        }
                    }
                    Ok(())
                })?
                .map_err(io::Error::other)?;
            return Ok(Some(Outcome::Settled { id }));
        }
        Ok(None)
    }

    fn replayer(&self, learned: &Learned) -> Replayer {
        Replayer::new(
            self.system.capabilities(),
            self.system.agents(),
            self.budget,
            self.planner.clone(),
            self.scratch.clone(),
        )
        .with_learned(learned.clone())
    }

    /// The next thing worth trying: a skill for a recurring sequence, or a
    /// lesson from a recent failure. Only tasks a replay can use count, or
    /// the candidate could never be shown to help them.
    async fn propose(
        &self,
        learned: &Learned,
        now: u64,
        spent: &mut u64,
    ) -> io::Result<Option<Candidate>> {
        let tasks = self.tasks.list();
        let specs: Vec<CapabilitySpec> = self
            .system
            .capabilities()
            .iter()
            .map(|c| c.spec().clone())
            .collect();
        let replayer = self.replayer(learned);
        let since = now.saturating_sub(self.config.retry_after_ms);
        let fresh = |task: &Task| !learned.tried(task.id.0, since);
        let replayable = |ids: Vec<TaskId>| {
            let (cases, _) = replay::cases(&tasks, &|n| replayer.offers(n), &Pick::Tasks(ids));
            cases
        };

        // A skill: a sequence of three or more successes the system has no
        // skill for yet.
        let skillful: Vec<&Task> = tasks.iter().filter(|t| fresh(t)).collect();
        for (sequence, ids) in recurring(&skillful, &specs) {
            let covered = learned.skills().iter().any(|m| m.capabilities == sequence);
            let risk = sequence
                .iter()
                .filter_map(|n| specs.iter().find(|s| s.name.as_str() == n))
                .map(|s| s.risk)
                .max()
                .unwrap_or(RiskLevel::ReadOnly);
            if covered || risk >= RiskLevel::Irreversible {
                continue;
            }
            let cases = replayable(ids.iter().map(|n| TaskId(*n)).collect());
            if cases.len() < 3 {
                continue;
            }
            let examples: Vec<&Task> = tasks
                .iter()
                .filter(|t| cases.iter().any(|c| c.task == t.id))
                .collect();
            let prompt = skill_prompt(&sequence, &examples, &specs);
            let answer = self.ask(SKILL_SYSTEM, &prompt, spent).await;
            let sources: Vec<u64> = cases.iter().map(|c| c.task.0).collect();
            let Some(answer) = answer else {
                self.remember_sources(&sources, now, "模型没有给出技能")?;
                continue;
            };
            let field = |key: &str| {
                answer
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            };
            let id = slug(&field("id"));
            return Ok(Some(Candidate {
                content: Content::Skill {
                    id: if id.is_empty() {
                        format!("skill-{}", learned.next_id + 1)
                    } else {
                        id
                    },
                    name: clip(&field("name"), 20),
                    description: clip(&field("description"), 120),
                    instructions: clip(&field("instructions"), 600),
                    capabilities: sequence,
                    max_risk: risk,
                },
                sources,
            }));
        }

        // A lesson: the latest failure that can be replayed.
        let mut failed: Vec<&Task> = tasks
            .iter()
            .filter(|t| t.state == TaskState::Failed && speaks_for_user(t) && fresh(t))
            .collect();
        failed.sort_by(|a, b| b.id.cmp(&a.id));
        for task in failed {
            if replayable(vec![task.id]).is_empty() {
                continue;
            }
            let answer = self
                .ask(LESSON_SYSTEM, &lesson_prompt(task, &specs), spent)
                .await;
            let lesson = answer
                .as_ref()
                .and_then(|a| a.get("lesson"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|l| !l.is_empty());
            let Some(lesson) = lesson else {
                self.remember_sources(&[task.id.0], now, "总结不出经验")?;
                continue;
            };
            return Ok(Some(Candidate {
                content: Content::Lesson {
                    text: lesson.to_string(),
                },
                sources: vec![task.id.0],
            }));
        }
        Ok(None)
    }

    fn remember_sources(&self, sources: &[u64], now: u64, reason: &str) -> io::Result<()> {
        self.learned
            .modify(|learned| {
                learned.remember_attempt(Attempt {
                    at_ms: now,
                    sources: sources.to_vec(),
                    reason: reason.into(),
                });
                Ok(())
            })?
            .map_err(io::Error::other)
    }

    async fn ask(&self, system: &str, user: &str, spent: &mut u64) -> Option<Value> {
        let (answer, usage) = self.model.complete_metered(system, user).await;
        if let Some(usage) = usage {
            *spent += usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0);
        }
        json_object(&answer.ok()?)
    }

    /// Replay with and without the candidate. The cases are the tasks it
    /// came from and the latest others, so it must help where it was meant
    /// to without hurting anywhere else.
    async fn evaluate(
        &self,
        without: &Learned,
        with: &Learned,
        candidate: &Candidate,
        spent: &mut u64,
    ) -> io::Result<Result<Evaluation, Stop>> {
        let tasks = self.tasks.list();
        // Offers are judged with the candidate in effect: a skill's own
        // delegation must count as something the replay can answer.
        let replayer = self.replayer(with);
        let offers = |n: &str| replayer.offers(n);
        let (mut cases, _) = replay::cases(
            &tasks,
            &offers,
            &Pick::Tasks(candidate.sources.iter().map(|n| TaskId(*n)).collect()),
        );
        let (recent, _) = replay::cases(&tasks, &offers, &Pick::Latest(self.config.cases));
        for case in recent {
            if !cases.iter().any(|c: &Case| c.task == case.task) {
                cases.push(case);
            }
        }
        let gate = self.config.gate;
        if cases.len() < gate.min_cases {
            return Ok(Err(Stop::Reason(format!(
                "可比较的用例只有 {} 个，至少要 {}",
                cases.len(),
                gate.min_cases
            ))));
        }
        let mut results = Vec::with_capacity(cases.len());
        for case in &cases {
            let mut base = Vec::new();
            let mut cand = Vec::new();
            for _ in 0..gate.runs.max(1) {
                if self.user_is_back() {
                    return Ok(Err(Stop::Yielded));
                }
                if *spent > self.config.tokens {
                    return Ok(Err(Stop::Reason(format!(
                        "这一轮的 token 预算（{}）用完了，没有评完",
                        self.config.tokens
                    ))));
                }
                let b = replayer.run_with(case, without).await?;
                let c = replayer.run_with(case, with).await?;
                *spent += b.replayed_tokens + c.replayed_tokens;
                base.push(b);
                cand.push(c);
            }
            results.push((base, cand));
        }
        Ok(Ok(judge(&gate, &results)))
    }
}

enum Stop {
    Yielded,
    Reason(String),
}

/// `improve.learned` (L0): what the system learned, in effect or withdrawn.
pub struct LearnedList {
    spec: CapabilitySpec,
    learned: Arc<LearnedStore>,
}

impl LearnedList {
    pub fn new(learned: Arc<LearnedStore>) -> Self {
        Self {
            spec: CapabilitySpec {
                name: agentos_core::CapabilityName::new("improve.learned"),
                description: "系统自己学到并采用的经验和技能：内容、来源、评测证据、是否还在生效。"
                    .into(),
                risk: RiskLevel::ReadOnly,
                input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            },
            learned,
        }
    }
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::Observing => "生效中（观察期）",
        Status::Active => "生效中",
        Status::RolledBack => "已撤回",
    }
}

#[async_trait::async_trait]
impl Capability for LearnedList {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, agentos_core::CapabilityError> {
        if args.as_object().is_none_or(|o| !o.is_empty()) {
            return Err(agentos_core::CapabilityError::InvalidArgs(
                "improve.learned 不需要参数".into(),
            ));
        }
        let (_, learned) = self.learned.current();
        let mut message = String::new();
        if learned.items.is_empty() {
            message.push_str("还没有学到什么。");
        }
        for item in learned.items.iter().rev() {
            let what = match &item.content {
                Content::Lesson { text } => format!("经验：{text}"),
                Content::Skill {
                    name, description, ..
                } => format!("技能「{name}」：{description}"),
            };
            message.push_str(&format!(
                "{} · {} · {what}",
                item.id,
                status_label(item.status)
            ));
            if let Some(reason) = item
                .evidence
                .pointer("/evaluation/reason")
                .and_then(Value::as_str)
            {
                message.push_str(&format!("\n  采用的理由：{reason}"));
            }
            if let Some(reason) = &item.reason {
                message.push_str(&format!("\n  撤回的原因：{reason}"));
            }
            message.push('\n');
        }
        Ok(json!({
            "items": learned.items,
            "message": message.trim_end(),
        }))
    }
}

/// `improve.rollback` (L1): withdraw something the system adopted. It is
/// the user's undo for adoptions no one approved, so it is cheap: reversible
/// and needs no PIN.
pub struct Rollback {
    spec: CapabilitySpec,
    learned: Arc<LearnedStore>,
    audit: Arc<AuditLog>,
}

impl Rollback {
    pub fn new(learned: Arc<LearnedStore>, audit: Arc<AuditLog>) -> Self {
        Self {
            spec: CapabilitySpec {
                name: agentos_core::CapabilityName::new("improve.rollback"),
                description: "撤回系统自己学到并采用的一条经验或一个技能，下一个任务起不再使用。"
                    .into(),
                risk: RiskLevel::Reversible,
                input_schema: json!({
                    "type": "object",
                    "properties": {"id": {"type": "string", "description": "例如 lesson-3、skill-4"}},
                    "required": ["id"],
                    "additionalProperties": false
                }),
            },
            learned,
            audit,
        }
    }
}

#[async_trait::async_trait]
impl Capability for Rollback {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, agentos_core::CapabilityError> {
        use agentos_core::CapabilityError;
        let object = args
            .as_object()
            .filter(|o| o.keys().all(|k| k == "id"))
            .ok_or_else(|| CapabilityError::InvalidArgs("只接受 id".into()))?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| CapabilityError::InvalidArgs("缺少 id".into()))?
            .to_string();
        let reason = "用户撤回";
        let done = self
            .learned
            .modify(|l| l.roll_back(&id, reason, now_ms()).map(|_| ()))
            .map_err(|e| CapabilityError::Failed(format!("没能撤回：{e}")))?;
        done.map_err(CapabilityError::Failed)?;
        self.audit
            .append(AuditEvent::ImprovementRolledBack {
                id: id.clone(),
                by: "user".into(),
                reason: reason.into(),
            })
            .map_err(|e| CapabilityError::Failed(format!("已撤回，但审计日志写不进去：{e}")))?;
        Ok(json!({"id": id, "message": format!("已撤回 {id}，下一个任务起不再使用")}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::{CapabilityError, CapabilityName, DeviceId, Intent, IntentId, Urgency};
    use agentos_planner::{Conversation, Plan, Step};
    use agentos_runtime::TaskStep;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;

    const LESSON: &str = "天气查不到时，用已经查到的信息直接回答，不要以失败的调用结束";

    struct Fake(CapabilitySpec);

    #[async_trait::async_trait]
    impl Capability for Fake {
        fn spec(&self) -> &CapabilitySpec {
            &self.0
        }
        async fn invoke(&self, _args: Value) -> Result<Value, CapabilityError> {
            panic!("a replay must never run a real capability");
        }
    }

    fn fake(name: &str, risk: RiskLevel) -> Arc<dyn Capability> {
        Arc::new(Fake(CapabilitySpec {
            name: CapabilityName::new(name),
            description: format!("{name} 的说明"),
            risk,
            input_schema: json!({"type": "object"}),
        }))
    }

    struct Fixed(Vec<Arc<dyn Capability>>);

    impl System for Fixed {
        fn capabilities(&self) -> Vec<Arc<dyn Capability>> {
            self.0.clone()
        }
        fn agents(&self) -> AgentDirectory {
            AgentDirectory::new()
        }
    }

    /// Plans "记一下" as one note, and the weather question as a notes read,
    /// a weather call and an answer, unless the lesson is in its context:
    /// then it answers from the notes, as the lesson says.
    struct Planner;

    #[async_trait::async_trait]
    impl agentos_planner::Planner for Planner {
        async fn plan(&self, _: &Intent, _: &[CapabilitySpec]) -> Plan {
            Plan::default()
        }
        async fn plan_turn(&self, conv: &Conversation<'_>) -> Plan {
            let call = |name: &str, args: Value| Plan {
                steps: vec![Step::new(CapabilityName::new(name), args)],
                ..Plan::default()
            };
            let answer = Plan {
                reply: Some("好了".into()),
                ..Plan::default()
            };
            let text = conv.intent.text.as_str();
            let rounds = conv.history.len();
            if let Some(note) = text.strip_prefix("记一下 ") {
                return if rounds == 0 {
                    call("notes.append", json!({"text": note}))
                } else {
                    answer
                };
            }
            let learned = conv.system.contains(LESSON);
            match (rounds, learned) {
                (0, _) => call("notes.list", json!({})),
                (1, false) => call("weather.get", json!({"city": "杭州"})),
                _ => answer,
            }
        }
    }

    struct Model {
        answer: String,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl TextModel for Model {
        async fn complete(&self, _system: &str, _user: &str) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.answer.clone())
        }
    }

    fn record(
        store: &TaskStore,
        text: &str,
        state: TaskState,
        steps: &[(&str, Value, bool, Value)],
        tainted: bool,
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
                for (name, args, ok, result) in steps {
                    let mut step = TaskStep::pending(CapabilityName::new(*name), args.clone());
                    step.state = if *ok {
                        StepState::Succeeded
                    } else {
                        StepState::Failed
                    };
                    step.result = Some(result.clone());
                    t.steps.push(step);
                }
                t.state = state;
                t.tainted = tainted;
            })
            .unwrap()
    }

    fn note(store: &TaskStore, i: usize) {
        let text = format!("买东西 {i}");
        record(
            store,
            &format!("记一下 {text}"),
            TaskState::Succeeded,
            &[(
                "notes.append",
                json!({"text": text}),
                true,
                json!({"message": "已记下"}),
            )],
            false,
        );
    }

    fn weather_failure(store: &TaskStore, tainted: bool) {
        record(
            store,
            "看看笔记里写的出发日，再查杭州天气",
            TaskState::Failed,
            &[
                (
                    "notes.list",
                    json!({}),
                    true,
                    json!({"message": "1. 周五出发"}),
                ),
                (
                    "weather.get",
                    json!({"city": "杭州"}),
                    false,
                    json!("执行失败：天气服务不可用"),
                ),
            ],
            tainted,
        );
    }

    static SCRATCH: AtomicU64 = AtomicU64::new(0);

    struct Setup {
        tasks: Arc<TaskStore>,
        audit: Arc<AuditLog>,
        learned: Arc<LearnedStore>,
        model: Arc<Model>,
        improver: Improver,
    }

    fn setup(answer: &str, config: Config, history: impl FnOnce(&TaskStore)) -> Setup {
        let tasks = Arc::new(TaskStore::memory());
        history(&tasks);
        let dir = std::env::temp_dir().join(format!(
            "agentos-learn-{}-{}",
            std::process::id(),
            SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        agentos_runtime::create_private_dir(&dir).unwrap();
        let audit = Arc::new(AuditLog::open(dir.join("audit.jsonl")).unwrap());
        let learned = Arc::new(LearnedStore::memory(Learned::default()));
        let model = Arc::new(Model {
            answer: answer.into(),
            calls: AtomicUsize::new(0),
        });
        let system = Arc::new(Fixed(vec![
            fake("notes.list", RiskLevel::ReadOnly),
            fake("notes.append", RiskLevel::Reversible),
            fake("weather.get", RiskLevel::ReadOnly),
        ]));
        let improver = Improver::new(
            tasks.clone(),
            audit.clone(),
            learned.clone(),
            system,
            model.clone(),
            Arc::new(Planner),
            "测试模型".into(),
            Budget::default(),
            config,
            dir.join("scratch"),
        );
        Setup {
            tasks,
            audit,
            learned,
            model,
            improver,
        }
    }

    fn small() -> Config {
        Config {
            gate: Gate {
                min_cases: 4,
                ..Gate::default()
            },
            observe_tasks: 5,
            ..Config::default()
        }
    }

    fn history(store: &TaskStore) {
        for i in 0..4 {
            note(store, i);
        }
        weather_failure(store, false);
        weather_failure(store, false);
    }

    fn events(audit: &AuditLog) -> Vec<AuditEvent> {
        AuditLog::replay(audit.path())
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect()
    }

    fn lesson_answer() -> String {
        json!({"lesson": LESSON}).to_string()
    }

    #[tokio::test]
    async fn a_lesson_that_replays_clearly_better_is_adopted_without_approval_and_audited() {
        let s = setup(&lesson_answer(), small(), history);
        let outcome = s.improver.cycle().await.unwrap();
        let Outcome::Applied { id, .. } = &outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(id, "lesson-1");
        let (_, learned) = s.learned.current();
        assert_eq!(learned.lessons(), [LESSON]);
        let item = learned.get("lesson-1").unwrap();
        assert_eq!(item.status, Status::Observing);
        assert_eq!(item.model.as_deref(), Some("测试模型"));
        let evaluation = &item.evidence["evaluation"];
        assert_eq!(evaluation["passed"], true);
        assert_eq!(evaluation["improved"].as_array().unwrap().len(), 2);
        assert!(evaluation["regressed"].as_array().unwrap().is_empty());
        // Guards "every adoption is on record": the audit log says what was
        // adopted and why, though no one approved it.
        assert!(events(&s.audit).iter().any(|e| matches!(
            e,
            AuditEvent::ImprovementApplied { id, kind, .. } if id == "lesson-1" && kind == "lesson"
        )));
        // One at a time: nothing new while it is watched.
        assert_eq!(
            s.improver.cycle().await.unwrap(),
            Outcome::Idle("上一个改进还在观察期".into())
        );
    }

    #[tokio::test]
    async fn an_adopted_lesson_is_withdrawn_when_the_tasks_after_it_do_worse() {
        let s = setup(&lesson_answer(), small(), history);
        assert!(matches!(
            s.improver.cycle().await.unwrap(),
            Outcome::Applied { .. }
        ));
        // The user's next tasks fail where they used to succeed.
        for _ in 0..5 {
            record(&s.tasks, "查天气", TaskState::Failed, &[], false);
        }
        let outcome = s.improver.cycle().await.unwrap();
        assert!(
            matches!(&outcome, Outcome::RolledBack { id, .. } if id == "lesson-1"),
            "{outcome:?}"
        );
        let (_, learned) = s.learned.current();
        assert!(learned.lessons().is_empty());
        assert_eq!(learned.get("lesson-1").unwrap().status, Status::RolledBack);
        assert!(events(&s.audit).iter().any(|e| matches!(
            e,
            AuditEvent::ImprovementRolledBack { id, by, .. } if id == "lesson-1" && by == "system"
        )));
    }

    #[tokio::test]
    async fn a_watch_without_harm_ends_with_the_lesson_kept() {
        let s = setup(&lesson_answer(), small(), history);
        assert!(matches!(
            s.improver.cycle().await.unwrap(),
            Outcome::Applied { .. }
        ));
        for i in 10..15 {
            note(&s.tasks, i);
        }
        let outcome = s.improver.cycle().await.unwrap();
        assert_eq!(
            outcome,
            Outcome::Settled {
                id: "lesson-1".into()
            }
        );
        assert_eq!(
            s.learned.current().1.get("lesson-1").unwrap().status,
            Status::Active
        );
    }

    #[tokio::test]
    async fn with_auto_apply_off_a_passing_candidate_is_only_reported() {
        let config = Config {
            auto: false,
            ..small()
        };
        let s = setup(&lesson_answer(), config, history);
        let outcome = s.improver.cycle().await.unwrap();
        assert!(matches!(outcome, Outcome::Withheld { .. }), "{outcome:?}");
        assert!(s.learned.current().1.lessons().is_empty());
        assert!(!events(&s.audit)
            .iter()
            .any(|e| matches!(e, AuditEvent::ImprovementApplied { .. })));
    }

    #[tokio::test]
    async fn a_candidate_without_a_clear_gain_is_not_adopted_and_its_task_is_not_retried() {
        // Only one failure to improve: below the two cases the gate asks for.
        let s = setup(&lesson_answer(), small(), |store| {
            for i in 0..4 {
                note(store, i);
            }
            weather_failure(store, false);
        });
        let outcome = s.improver.cycle().await.unwrap();
        assert!(
            matches!(&outcome, Outcome::Rejected { reason, .. } if reason == "没有明显变好"),
            "{outcome:?}"
        );
        assert!(s.learned.current().1.lessons().is_empty());
        // The failure is remembered as tried, so the next cycle does not
        // spend tokens on it again.
        assert_eq!(
            s.improver.cycle().await.unwrap(),
            Outcome::Idle("没有新的可以学的东西".into())
        );
        assert_eq!(s.model.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn outside_content_never_reaches_the_generator() {
        // Guards the injection path: a lesson written from a tainted task
        // would steer every later task, so such a task is never offered.
        let s = setup(&lesson_answer(), small(), |store| {
            for i in 0..4 {
                note(store, i);
            }
            weather_failure(store, true);
            weather_failure(store, true);
        });
        assert_eq!(
            s.improver.cycle().await.unwrap(),
            Outcome::Idle("没有新的可以学的东西".into())
        );
        assert_eq!(s.model.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn the_cycle_yields_when_the_user_comes_back_and_forgets_nothing() {
        let s = setup(&lesson_answer(), small(), history);
        // Something of the user's is queued.
        s.tasks
            .create(Intent {
                id: IntentId(0),
                source: DeviceId::new("unix:uid:1000"),
                speaker: None,
                text: "几点了".into(),
                urgency: Urgency::Normal,
                at_ms: 0,
            })
            .unwrap();
        assert_eq!(s.improver.cycle().await.unwrap(), Outcome::Yielded);
        let (_, learned) = s.learned.current();
        assert!(learned.items.is_empty());
        // Not counted as tried: it was interrupted, not judged.
        assert!(learned.attempts.is_empty());
    }

    #[tokio::test]
    async fn a_lesson_out_of_bounds_is_refused_before_anything_is_replayed() {
        let long = json!({"lesson": "长".repeat(300)}).to_string();
        let s = setup(&long, small(), history);
        let outcome = s.improver.cycle().await.unwrap();
        assert!(matches!(outcome, Outcome::Rejected { .. }), "{outcome:?}");
        assert!(s.learned.current().1.items.is_empty());
    }

    fn result(task: &str, verdict: Verdict, tokens: u64, steps: usize) -> CaseResult {
        CaseResult {
            task: task.into(),
            intent: String::new(),
            recorded: TaskState::Succeeded,
            replayed: TaskState::Succeeded,
            verdict,
            recorded_steps: steps,
            replayed_steps: steps,
            recorded_tokens: tokens,
            replayed_tokens: tokens,
            missing: Vec::new(),
            unanswered: false,
            message: None,
        }
    }

    fn case(
        task: &str,
        base: &[(Verdict, u64, usize)],
        cand: &[(Verdict, u64, usize)],
    ) -> (Vec<CaseResult>, Vec<CaseResult>) {
        let runs = |r: &[(Verdict, u64, usize)]| {
            r.iter().map(|(v, t, s)| result(task, *v, *t, *s)).collect()
        };
        (runs(base), runs(cand))
    }

    #[test]
    fn the_gate_wants_consistent_gains_and_no_regression_anywhere() {
        use Verdict::*;
        let gate = Gate {
            min_cases: 3,
            ..Gate::default()
        };
        let same = |t: &str| case(t, &[(Same, 100, 1); 3], &[(Same, 100, 1); 3]);
        let better = |t: &str| {
            case(
                t,
                &[(Same, 100, 2); 3],
                &[(Improved, 100, 1), (Improved, 100, 1), (Same, 100, 2)],
            )
        };
        let pass = judge(&gate, &[better("a"), better("b"), same("c")]);
        assert!(pass.passed, "{}", pass.reason);

        // Better in only one run of three is noise, not a gain.
        let lucky = case(
            "a",
            &[(Same, 100, 2); 3],
            &[(Improved, 100, 1), (Same, 100, 2), (Same, 100, 2)],
        );
        assert!(!judge(&gate, &[lucky, better("b"), same("c")]).passed);

        // A single regression in a single run sinks it, whatever else got better.
        let slip = case(
            "c",
            &[(Same, 100, 1); 3],
            &[(Same, 100, 1), (Regressed, 100, 1), (Same, 100, 1)],
        );
        let e = judge(&gate, &[better("a"), better("b"), slip]);
        assert!(!e.passed);
        assert_eq!(e.regressed, ["c"]);

        // Too few cases to say anything.
        assert!(!judge(&gate, &[better("a"), better("b")]).passed);

        // Gains that come from leaving the recording are not evidence.
        let off = |t: &str| case(t, &[(Same, 100, 1); 3], &[(Diverged, 100, 1); 3]);
        assert!(!judge(&gate, &[better("a"), better("b"), off("c"), off("d")]).passed);

        // Better at twice the cost is not clearly better.
        let pricey = |t: &str| {
            case(
                t,
                &[(Same, 100, 2); 3],
                &[(Improved, 300, 1), (Improved, 300, 1), (Improved, 300, 1)],
            )
        };
        assert!(!judge(&gate, &[pricey("a"), pricey("b"), same("c")]).passed);

        // Same outcomes, clearly leaner: worth having.
        let lean = |t: &str| case(t, &[(Same, 100, 4); 3], &[(Same, 60, 2); 3]);
        assert!(judge(&gate, &[lean("a"), lean("b"), lean("c")]).passed);
        let slightly = |t: &str| case(t, &[(Same, 100, 4); 3], &[(Same, 95, 4); 3]);
        assert!(!judge(&gate, &[slightly("a"), slightly("b"), slightly("c")]).passed);
    }

    #[tokio::test]
    async fn an_adopted_skill_is_an_agent_until_it_is_rolled_back() {
        use agentos_services::expression::ExpressionHub;
        use agentos_services::registry::CapabilityRegistry;
        let dir = std::env::temp_dir().join(format!(
            "agentos-learn-skill-{}-{}",
            std::process::id(),
            SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        agentos_runtime::create_private_dir(&dir).unwrap();
        let audit = Arc::new(AuditLog::open(dir.join("audit.jsonl")).unwrap());
        let learned = Arc::new(LearnedStore::memory(Learned::default()));
        let skill = Content::Skill {
            id: "trip".into(),
            name: "出差准备".into(),
            description: "准备出差".into(),
            instructions: "先看笔记再记下清单".into(),
            capabilities: vec!["notes.list".into(), "notes.append".into()],
            max_risk: RiskLevel::Reversible,
        };
        let item = Item {
            id: String::new(),
            content: skill.clone(),
            status: Status::Observing,
            sources: vec![],
            model: None,
            evidence: json!({}),
            applied_at_ms: 0,
            baseline: Rate::default(),
            settled_at_ms: None,
            rolled_back_at_ms: None,
            reason: None,
        };
        learned.modify(|l| l.adopt(skill, item)).unwrap().unwrap();
        let mut registry = CapabilityRegistry::new();
        let rollback = Arc::new(Rollback::new(learned.clone(), audit.clone()));
        registry.register(rollback.clone());
        let butler = Butler::new(
            agentos_core::AgentId::new("butler"),
            registry,
            agentos_policy::PolicyEngine::new(),
            Box::new(Planner),
            audit.clone(),
            ExpressionHub::new(audit.clone()),
        )
        .with_learned(learned.clone());
        let offered = |b: &Butler| {
            b.capabilities()
                .iter()
                .any(|s| s.name.as_str() == "agent.learned-trip")
        };
        assert!(offered(&butler));
        assert!(butler.configured_agents().get("learned-trip").is_none());
        // The user's undo: the next task no longer has it.
        rollback.invoke(json!({"id": "skill-1"})).await.unwrap();
        butler
            .handle(Intent {
                id: IntentId(0),
                source: DeviceId::new("unix:uid:1000"),
                speaker: None,
                text: "记一下 x".into(),
                urgency: Urgency::Normal,
                at_ms: 0,
            })
            .await;
        assert!(!offered(&butler));
        assert!(rollback.invoke(json!({"id": "skill-1"})).await.is_err());
        assert!(rollback
            .invoke(json!({"id": "skill-1", "extra": 1}))
            .await
            .is_err());
        assert!(events(&audit).iter().any(|e| matches!(
            e,
            AuditEvent::ImprovementRolledBack { by, .. } if by == "user"
        )));
        let _ = Mutex::new(());
    }

    #[test]
    fn configuration_comes_from_the_environment_and_typos_are_errors() {
        let from = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            Config::from_env(move |name| {
                pairs
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
            })
        };
        assert_eq!(from(&[]).unwrap(), Config::default());
        assert!(!from(&[("AGENTOS_IMPROVE_AUTO", "0")]).unwrap().auto);
        assert_eq!(
            from(&[("AGENTOS_IMPROVE_TOKENS", "5000")]).unwrap().tokens,
            5000
        );
        assert_eq!(
            from(&[("AGENTOS_IMPROVE_TOKENS", "0")]).unwrap().tokens,
            u64::MAX
        );
        assert!(from(&[("AGENTOS_IMPROVE_AUTO", "maybe")]).is_err());
        assert!(from(&[("AGENTOS_IMPROVE_TOKENS", "lots")]).is_err());
    }
}
