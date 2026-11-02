//! Tasks: the agent's processes.
//!
//! Every intent becomes a task with its own id, state and call stack (the
//! butler's frame at the bottom, a delegate's frame above it). Tasks are
//! independent: one waiting for the human does not block the next. Each task
//! is checkpointed to disk after every step, in the spirit of pi-durable, so
//! the agent is a continuous entity: after agentosd restarts, unfinished tasks
//! pick up where they stopped and finished ones remain as memory.
//!
//! A checkpoint is saved data, not a suspended stack: recovery rules decide
//! what happens to a step that was running when the process died (see
//! [`Task::recover`]).

use std::path::{Path, PathBuf};

use agentos_core::{now_ms, AgentId, CapabilitySpec, Intent, RiskLevel};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::blackboard::save;
use crate::planner::{Plan, Round, Step, ToolResult};

/// Finished tasks kept on disk (the agent's long-term task memory).
pub const KEEP_FINISHED: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Running,
    /// Waiting for the human to confirm a step.
    Waiting,
    Done,
    Cancelled,
}

impl TaskState {
    pub fn label(self) -> &'static str {
        match self {
            TaskState::Running => "进行中",
            TaskState::Waiting => "等待确认",
            TaskState::Done => "已完成",
            TaskState::Cancelled => "已取消",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, TaskState::Running | TaskState::Waiting)
    }
}

/// One agent's work on a task: the unit of the call stack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub task: u64,
    pub agent: AgentId,
    /// Display name: "管家" or the delegate's name.
    pub name: String,
    pub system: String,
    /// The human's intent for the butler; the task text for a delegate.
    pub intent: Intent,
    pub specs: Vec<CapabilitySpec>,
    pub history: Vec<Round>,
    /// The round being executed.
    pub plan: Option<Plan>,
    pub next: usize,
    pub results: Vec<ToolResult>,
    pub rounds: u32,
    pub max_rounds: u32,
    pub tainted: bool,
    /// Id of a step whose effect started but whose outcome is not committed.
    #[serde(default)]
    pub started: Option<String>,
}

impl Frame {
    pub fn is_butler(&self) -> bool {
        self.agent.as_str() == "butler"
    }

    /// Key that identifies the current round's plan for shells.
    pub fn plan_key(&self) -> String {
        format!("{}:{}:{}", self.task, self.agent, self.rounds)
    }

    pub fn current(&self) -> Option<&Step> {
        self.plan.as_ref().and_then(|p| p.steps.get(self.next))
    }

    /// Record `msg` as the current step's failure, skip the rest of the round
    /// (later steps may depend on it) and let the planner decide what next.
    pub fn fail_rest(&mut self, msg: &str) {
        let Some(plan) = &self.plan else { return };
        for (i, s) in plan.steps.iter().enumerate().skip(self.next) {
            self.results.push(ToolResult {
                id: s.id.clone(),
                ok: false,
                content: if i == self.next {
                    msg.to_string()
                } else {
                    "未执行：前面的步骤没有成功".into()
                },
            });
        }
        self.next = plan.steps.len();
    }

    /// What a delegate hands back when its planner gave no closing words.
    pub fn summary(&self) -> String {
        let done: Vec<String> = self
            .history
            .iter()
            .flat_map(|r| r.calls.iter().zip(&r.results))
            .filter(|(_, res)| res.ok)
            .map(|(c, _)| c.capability.to_string())
            .collect();
        if done.is_empty() {
            "没有完成任何步骤".into()
        } else {
            format!("已完成：{}", done.join("、"))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: u64,
    pub intent: Intent,
    pub state: TaskState,
    pub stack: Vec<Frame>,
    /// The butler's final answer, once done.
    pub answer: Option<String>,
    /// While waiting: the question and the shell event, to ask again after a restart.
    #[serde(default)]
    pub ask: Option<(String, Value)>,
    #[serde(default)]
    pub verify_identity: bool,
    pub created_ms: u64,
    pub updated_ms: u64,
}

impl Task {
    pub fn new(id: u64, intent: Intent, butler: Frame) -> Self {
        let now = now_ms();
        Self {
            id,
            intent,
            state: TaskState::Running,
            stack: vec![butler],
            answer: None,
            ask: None,
            verify_identity: false,
            created_ms: now,
            updated_ms: now,
        }
    }

    /// One line for lists and for the planner's memory.
    pub fn line(&self) -> String {
        let mut s = format!("#{} {}：{}", self.id, self.state.label(), self.intent.text);
        if let Some(a) = &self.answer {
            let a: String = a.chars().take(120).collect();
            s.push_str(" → ");
            s.push_str(&a);
        }
        s
    }

    /// Repair a task loaded after a crash. A step that had started but whose
    /// outcome was never committed is re-run only if it is read-only;
    /// otherwise its agent is told it was interrupted and decides what next
    /// (checkpointed, not exactly-once).
    pub fn recover(&mut self, risk_of: impl Fn(&Step) -> Option<RiskLevel>) {
        let Some(f) = self.stack.last_mut() else {
            return;
        };
        let Some(started) = f.started.take() else {
            return;
        };
        let Some(step) = f.current().cloned() else {
            return;
        };
        if step.id != started || risk_of(&step) == Some(RiskLevel::ReadOnly) {
            return;
        }
        f.results.push(ToolResult {
            id: step.id,
            ok: false,
            content:
                "执行这一步时 agentosd 中断了，不确定是否已经生效。请先查看现状，再决定要不要重做。"
                    .into(),
        });
        f.next += 1;
    }
}

/// Tasks on disk: one JSON file per task.
#[derive(Debug, Clone)]
pub struct TaskStore {
    dir: PathBuf,
}

impl TaskStore {
    pub fn open(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: u64) -> PathBuf {
        self.dir.join(format!("{id:08}.json"))
    }

    pub fn save(&self, task: &Task) -> std::io::Result<()> {
        save(&self.path(task.id), task)
    }

    /// All stored tasks, oldest first, and a note per unreadable file.
    pub fn load(&self) -> (Vec<Task>, Vec<String>) {
        let mut tasks = Vec::new();
        let mut notes = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return (tasks, notes);
        };
        for p in entries.filter_map(|e| e.ok().map(|e| e.path())) {
            if p.extension().map_or(true, |x| x != "json") {
                continue;
            }
            match std::fs::read_to_string(&p)
                .map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str::<Task>(&t).map_err(|e| e.to_string()))
            {
                Ok(t) => tasks.push(t),
                Err(e) => notes.push(format!("无法读取任务 {}：{e}", p.display())),
            }
        }
        tasks.sort_by_key(|t| t.id);
        (tasks, notes)
    }

    /// Drop the oldest finished tasks beyond `keep`.
    pub fn prune(&self, keep: usize) {
        let (tasks, _) = self.load();
        let finished: Vec<u64> = tasks
            .iter()
            .filter(|t| !t.state.is_active())
            .map(|t| t.id)
            .collect();
        if finished.len() > keep {
            for id in &finished[..finished.len() - keep] {
                let _ = std::fs::remove_file(self.path(*id));
            }
        }
    }
}
