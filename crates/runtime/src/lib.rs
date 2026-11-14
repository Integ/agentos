//! Durable, single-writer task state for the AgentOS runtime.
//!
//! A mutation is validated and durable before it becomes visible in memory:
//! it is appended to a journal, which is folded into an atomically replaced
//! snapshot from time to time. A restarted runtime interrupts unfinished tasks: it never guesses
//! whether an in-flight side effect happened, and never replays it automatically.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard};

use agentos_core::{now_ms, AgentId, CapabilityName, Intent, IntentId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

// 5: changes since the snapshot live in the journal. The snapshot format is
// unchanged, but an older program would read it without the journal and lose
// those changes silently; the new number makes it refuse instead.
const SCHEMA_VERSION: u32 = 5;
const STATE_FILE: &str = "tasks.json";
const JOURNAL_FILE: &str = "tasks.journal";
/// The fewest journal entries that trigger a fold into the snapshot.
#[cfg(not(test))]
const COMPACT_AFTER: usize = 1024;
// Small in tests, so that ordinary tests also cross folds.
#[cfg(test)]
const COMPACT_AFTER: usize = 4;

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct TaskId(pub u64);

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task-{}", self.0)
    }
}

impl FromStr for TaskId {
    type Err = std::num::ParseIntError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.strip_prefix("task-").unwrap_or(text).parse().map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Planning,
    Running,
    AwaitingConfirmation,
    /// The agent asked the user something it needs to know to go on. Not an
    /// outcome: the task stays here until it is answered or cancelled.
    AwaitingInput,
    /// Every planned step ran, or the planner answered without needing tools.
    Succeeded,
    /// The planner had no way to act on the intent. Nothing ran and nothing
    /// went wrong; clients must not present this as work that was done.
    Unhandled,
    Failed,
    Cancelled,
    Interrupted,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Unhandled | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Pending,
    Running,
    Succeeded,
    Failed,
    /// Planned in the same round as a step that failed, and therefore never
    /// started: later steps of a round may depend on earlier ones.
    Skipped,
}

impl StepState {
    /// The step is over: it will neither start nor change again.
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Skipped)
    }
}

/// One capability call of a task. Planning is a loop, so a task's steps grow
/// round by round; a step delegated to another agent is followed by the steps
/// that agent took on its behalf.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStep {
    /// Unique within the task. Empty in tasks stored before steps had ids.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// The agent that made the call when it is not the butler itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
    pub capability: CapabilityName,
    pub args: Value,
    pub state: StepState,
    pub result: Option<Value>,
}

impl TaskStep {
    /// A planned step of the butler that has not started.
    pub fn pending(capability: CapabilityName, args: Value) -> Self {
        Self {
            id: String::new(),
            agent: None,
            capability,
            args,
            state: StepState::Pending,
            result: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Confirmation {
    pub step: usize,
    pub expires_at_ms: u64,
    /// A fresh opaque token identifying the concrete confirmation request.
    #[serde(default)]
    pub token: String,
    /// The step is irreversible: approval must come with proof of identity
    /// (the PIN), not only from the right OS user.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub verify_identity: bool,
}

/// What a task has consumed so far. Counts are sums over every agent that
/// worked on the task, the judge included.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskUsage {
    /// Model requests made for this task.
    pub model_calls: u32,
    /// Requests among them whose token counts are not known (the request
    /// failed, or the service does not report usage). Their cost is missing
    /// from the token counts below, which are therefore a lower bound.
    pub unmetered_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Time spent working on the task, not counting time it waited for the
    /// human or in the queue.
    pub active_ms: u64,
}

impl TaskUsage {
    pub fn tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// What an agent asked the user. The asking is itself a step of the task;
/// the answer becomes that step's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub step: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: TaskId,
    /// The conversation this task belongs to, named by its first task. A task
    /// that starts a conversation is its own thread. Zero only in snapshots
    /// stored before threads existed; it is replaced by `id` when loaded.
    #[serde(default)]
    pub thread: TaskId,
    pub intent: Intent,
    pub state: TaskState,
    pub steps: Vec<TaskStep>,
    pub tainted: bool,
    pub confirmation: Option<Confirmation>,
    /// The question a task in `AwaitingInput` is waiting on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<Question>,
    #[serde(default)]
    pub usage: TaskUsage,
    pub message: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema_version: u32,
    next_id: u64,
    #[serde(default)]
    revision: u64,
    tasks: Vec<Task>,
}

/// What is written as a snapshot: the current state, borrowed rather than
/// copied, since it can hold the whole task history.
#[derive(Serialize)]
struct SnapshotView<'a> {
    schema_version: u32,
    next_id: u64,
    revision: u64,
    tasks: Vec<&'a Task>,
}

/// One line of the journal: a task as it is after one change, and the
/// counters as they are after it. Replaying the lines above the snapshot's
/// revision, in order, gives the state the store last acknowledged.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalEntry<T> {
    revision: u64,
    next_id: u64,
    task: T,
}

#[derive(Debug, Clone)]
struct State {
    next_id: u64,
    revision: u64,
    tasks: BTreeMap<TaskId, Task>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            next_id: 1,
            revision: 0,
            tasks: BTreeMap::new(),
        }
    }
}

/// What [`State::apply`] replaced, so a change that could not be made
/// durable can be taken back.
struct Undo {
    id: TaskId,
    task: Option<Task>,
    next_id: u64,
    revision: u64,
}

impl State {
    fn from_snapshot(mut snapshot: Snapshot) -> io::Result<Self> {
        for task in &mut snapshot.tasks {
            if task.thread.0 == 0 {
                task.thread = task.id;
            }
        }
        validate_snapshot(&snapshot)?;
        Ok(Self {
            next_id: snapshot.next_id,
            revision: if snapshot.schema_version == 1 {
                snapshot.revision.max(snapshot.next_id - 1)
            } else {
                snapshot.revision
            },
            tasks: snapshot
                .tasks
                .into_iter()
                .map(|task| (task.id, task))
                .collect(),
        })
    }

    fn apply(&mut self, task: Task, next_id: u64, revision: u64) -> Undo {
        let undo = Undo {
            id: task.id,
            task: self.tasks.insert(task.id, task),
            next_id: self.next_id,
            revision: self.revision,
        };
        self.next_id = next_id;
        self.revision = revision;
        undo
    }

    fn undo(&mut self, undo: Undo) {
        match undo.task {
            Some(task) => {
                self.tasks.insert(undo.id, task);
            }
            None => {
                self.tasks.remove(&undo.id);
            }
        }
        self.next_id = undo.next_id;
        self.revision = undo.revision;
    }
}

/// How the journal stands since the last snapshot.
#[derive(Debug, Default)]
struct Journal {
    entries: usize,
    /// An append failed, so the file may end in a partial line. Nothing may
    /// be appended after it; the next change is written as a snapshot.
    broken: bool,
}

#[derive(Debug)]
struct Inner {
    state: State,
    journal: Journal,
}

/// A task store owns an exclusive process lock until it is dropped.
///
/// Methods take `&self`, so a daemon may share one store via `Arc<TaskStore>`.
/// Opening the same directory a second time fails instead of risking lost writes.
///
/// On disk the state is a snapshot (`tasks.json`) and a journal of changes
/// made since (`tasks.journal`). A change appends the one task it touched,
/// so its cost does not grow with the history; the journal is folded into
/// the snapshot once it holds about as many entries as there are tasks.
pub struct TaskStore {
    directory: Option<PathBuf>,
    inner: Mutex<Inner>,
    _lock: Option<File>,
    changes: broadcast::Sender<TaskChange>,
}

/// A bounded live invalidation hint. The task store remains authoritative;
/// slow or reconnected subscribers must refresh pages instead of replaying work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskChange {
    pub revision: u64,
    pub id: TaskId,
}

impl TaskStore {
    pub fn open(directory: impl AsRef<Path>) -> io::Result<Self> {
        let directory = directory.as_ref().to_path_buf();
        create_private_dir(&directory)?;
        let lock = exclusive_lock(&directory.join(".lock"))?;
        let mut state = match fs::read(directory.join(STATE_FILE)) {
            Ok(bytes) => {
                let snapshot = serde_json::from_slice(&bytes).map_err(invalid_data)?;
                State::from_snapshot(snapshot)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => State::default(),
            Err(error) => return Err(error),
        };
        // Nothing is written until both files have read back cleanly, so a
        // damaged store is left exactly as it was found.
        replay_journal(&directory, &mut state)?;
        let recovered_at = now_ms();
        let mut recovered = false;
        for task in state.tasks.values_mut() {
            if !task.state.is_terminal() {
                recovered = true;
                task.state = TaskState::Interrupted;
                task.confirmation = None;
                task.question = None;
                task.updated_at_ms = recovered_at.max(task.updated_at_ms);
                task.message = Some(
                    "守护进程已重启；未完成任务已中断，不会自动重复执行可能已发生的副作用。".into(),
                );
            }
        }
        if recovered {
            state.revision = next_revision(state.revision)?;
        }
        validate_state(&state)?;
        // Also persists an empty initial snapshot, detecting unwritable state
        // directories before the daemon accepts any intents. The snapshot
        // comes first: until the journal is emptied, its lines are at or
        // below the snapshot's revision and a replay skips them.
        persist(&directory, &state)?;
        reset_journal(&directory)?;
        Ok(Self {
            directory: Some(directory),
            inner: Mutex::new(Inner {
                state,
                journal: Journal::default(),
            }),
            _lock: Some(lock),
            changes: broadcast::channel(256).0,
        })
    }

    /// An ephemeral store, intended for deterministic tests and embedding.
    pub fn memory() -> Self {
        Self {
            directory: None,
            inner: Mutex::new(Inner {
                state: State::default(),
                journal: Journal::default(),
            }),
            _lock: None,
            changes: broadcast::channel(256).0,
        }
    }

    fn locked(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A task that starts a new thread.
    pub fn create(&self, intent: Intent) -> io::Result<Task> {
        self.create_in(intent, None)
    }

    /// A task that continues `thread`, or starts a new one. Continuing a
    /// thread that does not exist is `NotFound`; continuing one that another
    /// source started is `PermissionDenied`, so conversations never mix.
    pub fn create_in(&self, mut intent: Intent, thread: Option<TaskId>) -> io::Result<Task> {
        let mut guard = self.locked();
        if let Some(thread) = thread {
            let root = guard
                .state
                .tasks
                .get(&thread)
                .filter(|root| root.thread == thread)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, format!("unknown thread: {thread}"))
                })?;
            if root.intent.source != intent.source || root.intent.speaker != intent.speaker {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "a thread can only be continued by the source that started it",
                ));
            }
        }
        let id = TaskId(guard.state.next_id);
        let next_id = guard
            .state
            .next_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("task id space exhausted"))?;
        intent.id = IntentId(id.0);
        let at_ms = now_ms();
        let task = Task {
            id,
            thread: thread.unwrap_or(id),
            intent,
            state: TaskState::Queued,
            steps: Vec::new(),
            tainted: false,
            confirmation: None,
            question: None,
            usage: TaskUsage::default(),
            message: None,
            created_at_ms: at_ms,
            updated_at_ms: at_ms,
        };
        let revision = next_revision(guard.state.revision)?;
        self.commit(&mut guard, task.clone(), next_id, revision)?;
        let _ = self.changes.send(TaskChange { revision, id });
        Ok(task)
    }

    pub fn get(&self, id: TaskId) -> Option<Task> {
        self.locked().state.tasks.get(&id).cloned()
    }

    pub fn list(&self) -> Vec<Task> {
        self.locked().state.tasks.values().cloned().collect()
    }

    /// Page history by descending task ID. `before` is exclusive and the next
    /// cursor is the oldest returned ID only when a further matching row exists.
    /// The revision is captured under the same lock as the page contents.
    pub fn page(
        &self,
        before: Option<TaskId>,
        limit: usize,
        state: Option<TaskState>,
    ) -> (Vec<Task>, Option<TaskId>, u64) {
        self.page_in(before, limit, state, None, None)
    }

    /// [`Self::page`], optionally restricted to the tasks of one thread
    /// and to those submitted from one source.
    pub fn page_in(
        &self,
        before: Option<TaskId>,
        limit: usize,
        state: Option<TaskState>,
        thread: Option<TaskId>,
        source: Option<&str>,
    ) -> (Vec<Task>, Option<TaskId>, u64) {
        let guard = self.locked();
        if limit == 0 {
            return (Vec::new(), None, guard.state.revision);
        }
        let bound = before.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
        let mut matching = guard
            .state
            .tasks
            .range((std::ops::Bound::Unbounded, bound))
            .rev()
            .map(|(_, task)| task)
            .filter(|task| state.is_none_or(|state| task.state == state))
            .filter(|task| thread.is_none_or(|thread| task.thread == thread))
            .filter(|task| source.is_none_or(|source| task.intent.source.as_str() == source));
        let tasks: Vec<_> = matching.by_ref().take(limit).cloned().collect();
        let next = if matching.next().is_some() {
            tasks.last().map(|task| task.id)
        } else {
            None
        };
        (tasks, next, guard.state.revision)
    }

    /// Earlier tasks of the same thread as `id`, oldest first, at most the
    /// `limit` most recent ones.
    pub fn earlier_in_thread(&self, id: TaskId, limit: usize) -> Vec<Task> {
        let guard = self.locked();
        let Some(thread) = guard.state.tasks.get(&id).map(|task| task.thread) else {
            return Vec::new();
        };
        let mut earlier: Vec<Task> = guard
            .state
            .tasks
            .range(..id)
            .rev()
            .map(|(_, task)| task)
            .filter(|task| task.thread == thread)
            .take(limit)
            .cloned()
            .collect();
        earlier.reverse();
        earlier
    }

    /// The thread a task belongs to.
    pub fn thread_of(&self, id: TaskId) -> Option<TaskId> {
        self.locked().state.tasks.get(&id).map(|task| task.thread)
    }

    pub fn revision(&self) -> u64 {
        self.locked().state.revision
    }

    /// Subscribe before reading a page/revision, then discard hints at or below
    /// that revision to avoid a gap between the initial snapshot and live data.
    pub fn subscribe(&self) -> broadcast::Receiver<TaskChange> {
        self.changes.subscribe()
    }

    pub fn update(&self, id: TaskId, change: impl FnOnce(&mut Task)) -> io::Result<Task> {
        let mut guard = self.locked();
        let mut task = guard
            .state
            .tasks
            .get(&id)
            .cloned()
            .ok_or_else(|| missing_task(id))?;
        let (created_at_ms, thread) = (task.created_at_ms, task.thread);
        let previous_updated_at_ms = task.updated_at_ms;
        change(&mut task);
        // A task's thread is fixed too: other tasks name it as their root.
        if task.id != id
            || task.intent.id.0 != id.0
            || task.created_at_ms != created_at_ms
            || task.thread != thread
        {
            return Err(invalid_data(
                "task identity, thread and creation time are immutable",
            ));
        }
        task.updated_at_ms = now_ms()
            .max(task.updated_at_ms)
            .max(previous_updated_at_ms)
            .max(created_at_ms);
        let next_id = guard.state.next_id;
        let revision = next_revision(guard.state.revision)?;
        self.commit(&mut guard, task.clone(), next_id, revision)?;
        let _ = self.changes.send(TaskChange { revision, id });
        Ok(task)
    }

    pub fn cancel(&self, id: TaskId) -> io::Result<Task> {
        // A terminal task is returned unchanged, including its timestamps.
        let mut guard = self.locked();
        let mut task = guard
            .state
            .tasks
            .get(&id)
            .cloned()
            .ok_or_else(|| missing_task(id))?;
        if task.state.is_terminal() {
            return Ok(task);
        }
        task.state = TaskState::Cancelled;
        task.confirmation = None;
        task.question = None;
        task.updated_at_ms = now_ms().max(task.updated_at_ms);
        task.message =
            Some("任务已取消；后续步骤不会执行，已开始的能力调用可能仍产生结果。".into());
        let next_id = guard.state.next_id;
        let revision = next_revision(guard.state.revision)?;
        self.commit(&mut guard, task.clone(), next_id, revision)?;
        let _ = self.changes.send(TaskChange { revision, id });
        Ok(task)
    }

    /// The task's current state, without copying the task.
    pub fn state(&self, id: TaskId) -> Option<TaskState> {
        self.locked().state.tasks.get(&id).map(|task| task.state)
    }

    pub fn is_cancelled(&self, id: TaskId) -> bool {
        self.state(id) == Some(TaskState::Cancelled)
    }

    /// Make one task's new state durable, and only then visible. A change
    /// that cannot be written leaves memory as it was.
    fn commit(&self, inner: &mut Inner, task: Task, next_id: u64, revision: u64) -> io::Result<()> {
        validate_change(&inner.state, &task, next_id, revision)?;
        let Some(directory) = &self.directory else {
            inner.state.apply(task, next_id, revision);
            return Ok(());
        };
        // Folding the journal in once it holds as many entries as there are
        // tasks keeps the full rewrite's cost, spread over those changes,
        // constant per change.
        let due = inner.journal.entries >= inner.state.tasks.len().max(COMPACT_AFTER);
        if inner.journal.broken || due {
            let undo = inner.state.apply(task, next_id, revision);
            if let Err(error) = persist(directory, &inner.state) {
                inner.state.undo(undo);
                return Err(error);
            }
            // The change is durable with the snapshot. Old journal lines are
            // at or below its revision, so emptying the journal is only
            // housekeeping; if it fails, the next change tries again.
            inner.journal = match reset_journal(directory) {
                Ok(()) => Journal::default(),
                Err(_) => Journal {
                    entries: inner.journal.entries,
                    broken: true,
                },
            };
            return Ok(());
        }
        let entry = JournalEntry {
            revision,
            next_id,
            task: &task,
        };
        let mut line = serde_json::to_vec(&entry).map_err(invalid_data)?;
        line.push(b'\n');
        if let Err(error) = append_journal(directory, &line) {
            inner.journal.broken = true;
            return Err(error);
        }
        inner.journal.entries += 1;
        inner.state.apply(task, next_id, revision);
        Ok(())
    }
}

impl Drop for TaskStore {
    /// A clean shutdown leaves everything in the snapshot. This is a
    /// convenience, not a guarantee: if it fails, the journal still holds
    /// every acknowledged change and the next open replays it.
    fn drop(&mut self) {
        let Some(directory) = &self.directory else {
            return;
        };
        let inner = self.inner.get_mut().unwrap_or_else(|p| p.into_inner());
        if inner.journal.entries == 0 && !inner.journal.broken {
            return;
        }
        if persist(directory, &inner.state).is_ok() {
            let _ = reset_journal(directory);
        }
    }
}

/// Create `directory` and any missing parents readable only by this user.
///
/// The mode is explicit rather than left to the umask: desktop sessions on
/// Debian and Ubuntu run with umask 002, which would make the directory
/// group-writable. It holds tasks, notes and the audit log, and the control
/// socket beside them refuses a parent that others can write to. A directory
/// that already exists is left as it is.
pub fn create_private_dir(directory: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(directory)
    }
}

fn missing_task(id: TaskId) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("unknown task: {id}"))
}

fn next_revision(revision: u64) -> io::Result<u64> {
    revision
        .checked_add(1)
        .ok_or_else(|| io::Error::other("task revision space exhausted"))
}

fn invalid_data(error: impl fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn validate_snapshot(snapshot: &Snapshot) -> io::Result<()> {
    if !(1..=SCHEMA_VERSION).contains(&snapshot.schema_version) {
        return Err(invalid_data("unsupported task snapshot schema_version"));
    }
    validate_tasks(
        snapshot.schema_version,
        snapshot.next_id,
        snapshot.revision,
        snapshot.tasks.iter(),
    )
}

fn validate_state(state: &State) -> io::Result<()> {
    validate_tasks(
        SCHEMA_VERSION,
        state.next_id,
        state.revision,
        state.tasks.values(),
    )
}

fn validate_counters(schema_version: u32, next_id: u64, revision: u64) -> io::Result<()> {
    if next_id == 0 {
        return Err(invalid_data("next_id must be positive"));
    }
    if schema_version >= 2 && revision < next_id - 1 {
        return Err(invalid_data("task revision precedes allocated task ids"));
    }
    Ok(())
}

fn validate_tasks<'a>(
    schema_version: u32,
    next_id: u64,
    revision: u64,
    tasks: impl Iterator<Item = &'a Task> + Clone,
) -> io::Result<()> {
    validate_counters(schema_version, next_id, revision)?;
    let roots: BTreeSet<TaskId> = tasks
        .clone()
        .filter(|task| task.thread == task.id)
        .map(|task| task.id)
        .collect();
    let mut ids = BTreeSet::new();
    for task in tasks {
        if !ids.insert(task.id) {
            return Err(invalid_data("invalid, duplicate, or unallocated task id"));
        }
        validate_task(task, next_id, |thread| roots.contains(&thread))?;
    }
    Ok(())
}

/// One change, checked against the state it applies to. Only the changed
/// task can have broken anything: the others were valid and `update` keeps
/// identity and thread fixed, so no other task loses its root.
fn validate_change(state: &State, task: &Task, next_id: u64, revision: u64) -> io::Result<()> {
    validate_counters(SCHEMA_VERSION, next_id, revision)?;
    if next_id < state.next_id || revision <= state.revision {
        return Err(invalid_data("task counters must only move forward"));
    }
    validate_task(task, next_id, |thread| {
        if thread == task.id {
            task.thread == task.id
        } else {
            state
                .tasks
                .get(&thread)
                .is_some_and(|root| root.thread == root.id)
        }
    })
}

fn validate_task(task: &Task, next_id: u64, is_root: impl Fn(TaskId) -> bool) -> io::Result<()> {
    // A thread is named by its first task, which must exist.
    if task.thread > task.id || !is_root(task.thread) {
        return Err(invalid_data("task belongs to a thread that does not exist"));
    }
    if task.id.0 == 0 || task.id.0 >= next_id {
        return Err(invalid_data("invalid, duplicate, or unallocated task id"));
    }
    if task.intent.id.0 != task.id.0 || task.updated_at_ms < task.created_at_ms {
        return Err(invalid_data("invalid task identity or timestamps"));
    }
    if task
        .steps
        .iter()
        .any(|step| step.capability.as_str().trim().is_empty())
    {
        return Err(invalid_data("task step capability must not be empty"));
    }
    // Steps are not strictly sequential: a failed step may be followed by
    // another round, and a delegating step stays running while its agent's
    // steps are recorded after it. What must hold is that step ids are
    // distinct, and that a task that succeeded left nothing unsettled.
    let mut step_ids = BTreeSet::new();
    if task
        .steps
        .iter()
        .any(|step| !step.id.is_empty() && !step_ids.insert(step.id.as_str()))
    {
        return Err(invalid_data("duplicate task step id"));
    }
    if task.state == TaskState::Succeeded && task.steps.iter().any(|step| !step.state.is_settled())
    {
        return Err(invalid_data("succeeded task has incomplete steps"));
    }
    if task.state == TaskState::Unhandled && !task.steps.is_empty() {
        return Err(invalid_data("unhandled task has planned steps"));
    }
    match (&task.confirmation, task.state) {
        (Some(confirmation), TaskState::AwaitingConfirmation) => {
            if confirmation.expires_at_ms <= task.created_at_ms
                || confirmation.step >= task.steps.len()
                || task.steps[confirmation.step].state != StepState::Pending
            {
                return Err(invalid_data("invalid pending confirmation"));
            }
        }
        (None, TaskState::AwaitingConfirmation) => {
            return Err(invalid_data("awaiting task has no confirmation"));
        }
        (Some(_), _) => return Err(invalid_data("confirmation attached to non-waiting task")),
        (None, _) => {}
    }
    match (&task.question, task.state) {
        (Some(question), TaskState::AwaitingInput) => {
            if question.text.trim().is_empty()
                || question.step >= task.steps.len()
                || task.steps[question.step].state != StepState::Pending
            {
                return Err(invalid_data("invalid pending question"));
            }
        }
        (None, TaskState::AwaitingInput) => {
            return Err(invalid_data("task awaiting input has no question"));
        }
        (Some(_), _) => return Err(invalid_data("question attached to non-waiting task")),
        (None, _) => {}
    }
    Ok(())
}

/// Apply the journal's changes above the snapshot's revision to `state`.
fn replay_journal(directory: &Path, state: &mut State) -> io::Result<()> {
    let bytes = match fs::read(directory.join(JOURNAL_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    // A crash during an append leaves a last line without its newline. That
    // change was never acknowledged, so it is dropped. Anything else that does
    // not read back is damage: the store refuses to open rather than guess.
    let complete = match bytes.iter().rposition(|&byte| byte == b'\n') {
        Some(end) => &bytes[..end],
        None => return Ok(()),
    };
    // Lines are written in revision order. Some may already be in the
    // snapshot (a fold whose emptying of the journal failed); every line
    // after them must be the very next change.
    let mut last = None;
    for line in complete.split(|&byte| byte == b'\n') {
        let entry: JournalEntry<Task> = serde_json::from_slice(line).map_err(invalid_data)?;
        if last.is_some_and(|last| entry.revision <= last) {
            return Err(invalid_data("task journal goes back in revision"));
        }
        last = Some(entry.revision);
        if entry.revision <= state.revision {
            continue;
        }
        if Some(entry.revision) != state.revision.checked_add(1) {
            return Err(invalid_data("task journal skips a revision"));
        }
        validate_change(state, &entry.task, entry.next_id, entry.revision)?;
        state.apply(entry.task, entry.next_id, entry.revision);
    }
    Ok(())
}

fn append_journal(directory: &Path, line: &[u8]) -> io::Result<()> {
    // Opened by path for each change rather than held open: a journal that
    // has gone missing or been replaced fails here, instead of the change
    // going into a file nobody will read.
    let mut file = OpenOptions::new()
        .append(true)
        .open(directory.join(JOURNAL_FILE))?;
    let before = file.metadata()?.len();
    let result = file.write_all(line).and_then(|()| file.sync_data());
    if result.is_err() {
        // Best effort: the store also stops appending until the next fold.
        let _ = file.set_len(before);
    }
    result
}

/// Empty the journal, creating it if needed, durably.
fn reset_journal(directory: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(directory.join(JOURNAL_FILE))?.sync_all()?;
    agentos_core::durable::sync_dir(directory)
}

/// Atomically replace the snapshot with `state`. Callers validate first.
fn persist(directory: &Path, state: &State) -> io::Result<()> {
    let snapshot = SnapshotView {
        schema_version: SCHEMA_VERSION,
        next_id: state.next_id,
        revision: state.revision,
        tasks: state.tasks.values().collect(),
    };
    let mut bytes = serde_json::to_vec_pretty(&snapshot).map_err(invalid_data)?;
    bytes.push(b'\n');
    agentos_core::durable::write_atomic(&directory.join(STATE_FILE), &bytes)
}

#[cfg(unix)]
fn exclusive_lock(path: &Path) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    use std::os::raw::c_int;
    use std::os::unix::fs::OpenOptionsExt;

    extern "C" {
        fn flock(fd: c_int, operation: c_int) -> c_int;
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    // SAFETY: `file` owns a live descriptor. flock retains no pointer and the
    // descriptor stays open in TaskStore for the lifetime of the lock.
    if unsafe { flock(file.as_raw_fd(), 2 | 4) } != 0 {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            "task state directory is already locked by another runtime",
        ));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn exclusive_lock(_path: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "persistent task stores require Unix file locking",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    use agentos_core::{DeviceId, Urgency};
    use serde_json::json;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "agentos-runtime-{}-{}-{}",
                std::process::id(),
                now_ms(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn intent() -> Intent {
        Intent {
            id: IntentId(99),
            source: DeviceId::new("keyboard"),
            speaker: None,
            text: "记一下 买牛奶".into(),
            urgency: Urgency::Normal,
            at_ms: now_ms(),
        }
    }

    fn step(state: StepState) -> TaskStep {
        TaskStep {
            state,
            ..TaskStep::pending(
                CapabilityName::new("notes.append"),
                json!({"text": "买牛奶"}),
            )
        }
    }

    /// A copy of the files as they are on disk now, as a crash would leave them.
    fn crash_copy(from: &Path) -> TempDir {
        let copy = TempDir::new();
        for file in [STATE_FILE, JOURNAL_FILE] {
            fs::copy(from.join(file), copy.0.join(file)).unwrap();
        }
        copy
    }

    #[test]
    fn a_change_appends_one_task_instead_of_rewriting_the_history() {
        // Guards the cost of a step: it must not grow with the history.
        let dir = TempDir::new();
        let store = TaskStore::open(&dir.0).unwrap();
        let first = store.create(intent()).unwrap();
        let second = store.create(intent()).unwrap();
        store
            .update(first.id, |task| task.state = TaskState::Succeeded)
            .unwrap();
        // Still below the fold: the snapshot is the one written at open.
        let snapshot: Value =
            serde_json::from_slice(&fs::read(dir.0.join(STATE_FILE)).unwrap()).unwrap();
        assert!(snapshot["tasks"].as_array().unwrap().is_empty());
        let journal = fs::read(dir.0.join(JOURNAL_FILE)).unwrap();
        let lines: Vec<Value> = journal
            .split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1]["task"]["id"], json!(second.id.0));
        assert_eq!(lines[2]["task"]["state"], json!("succeeded"));
        assert_eq!(lines[2]["revision"], json!(store.revision()));
    }

    #[test]
    fn acknowledged_changes_survive_a_crash_and_a_torn_last_line_is_dropped() {
        // Guards durability: what a change returned Ok for is on disk.
        let dir = TempDir::new();
        let store = TaskStore::open(&dir.0).unwrap();
        let task = store.create(intent()).unwrap();
        store
            .update(task.id, |task| task.state = TaskState::Succeeded)
            .unwrap();
        let crashed = crash_copy(&dir.0);
        // A crash mid-append leaves part of a line that was never acknowledged.
        let mut journal = OpenOptions::new()
            .append(true)
            .open(crashed.0.join(JOURNAL_FILE))
            .unwrap();
        journal.write_all(br#"{"revision":99,"next_id":"#).unwrap();
        drop(journal);
        let reopened = TaskStore::open(&crashed.0).unwrap();
        assert_eq!(reopened.get(task.id).unwrap().state, TaskState::Succeeded);
        assert_eq!(reopened.revision(), store.revision());
        assert_eq!(reopened.create(intent()).unwrap().id, TaskId(2));
        // Opening folded everything into the snapshot and emptied the journal.
        drop(reopened);
        let saved: Value =
            serde_json::from_slice(&fs::read(crashed.0.join(STATE_FILE)).unwrap()).unwrap();
        assert_eq!(saved["tasks"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn many_changes_fold_into_the_snapshot_and_still_read_back_after_a_crash() {
        // Guards the fold: the journal stays bounded and nothing is lost
        // across it, including journal lines the snapshot already holds.
        let dir = TempDir::new();
        let store = TaskStore::open(&dir.0).unwrap();
        let mut ids = Vec::new();
        for _ in 0..(COMPACT_AFTER * 3 + 1) {
            ids.push(store.create(intent()).unwrap().id);
        }
        for id in &ids {
            store
                .update(*id, |task| task.state = TaskState::Succeeded)
                .unwrap();
        }
        let lines = fs::read(dir.0.join(JOURNAL_FILE))
            .unwrap()
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count();
        assert!(lines <= ids.len(), "{lines} journal lines");
        let crashed = crash_copy(&dir.0);
        // A fold whose emptying of the journal never happened leaves lines
        // the snapshot already holds; they are skipped, not applied twice.
        let stale = fs::read(dir.0.join(JOURNAL_FILE)).unwrap();
        let reopened = TaskStore::open(&crashed.0).unwrap();
        assert_eq!(reopened.revision(), store.revision());
        assert!(reopened
            .list()
            .iter()
            .all(|task| task.state == TaskState::Succeeded));
        drop(reopened);
        fs::write(crashed.0.join(JOURNAL_FILE), &stale).unwrap();
        let reopened = TaskStore::open(&crashed.0).unwrap();
        assert_eq!(reopened.revision(), store.revision());
        assert_eq!(reopened.list().len(), ids.len());
    }

    #[test]
    fn a_damaged_journal_line_refuses_to_open_and_leaves_both_files_alone() {
        // Guards against guessing: only a torn last line may be dropped.
        let dir = TempDir::new();
        let store = TaskStore::open(&dir.0).unwrap();
        let task = store.create(intent()).unwrap();
        store
            .update(task.id, |task| task.state = TaskState::Succeeded)
            .unwrap();
        let crashed = crash_copy(&dir.0);
        drop(store);
        let journal = fs::read(crashed.0.join(JOURNAL_FILE)).unwrap();
        let first_end = journal.iter().position(|&byte| byte == b'\n').unwrap();
        let mut cases = Vec::new();
        let mut garbled = journal.clone();
        garbled[1] = b'#';
        cases.push(garbled);
        // A complete line that goes backwards is damage too.
        let mut reordered = journal[first_end + 1..].to_vec();
        reordered.extend_from_slice(&journal[..=first_end]);
        reordered.extend_from_slice(&journal[first_end + 1..]);
        cases.push(reordered);
        // So is one that leaves out a change it was acknowledged after.
        cases.push(journal[first_end + 1..].to_vec());
        let snapshot = fs::read(crashed.0.join(STATE_FILE)).unwrap();
        for bytes in cases {
            fs::write(crashed.0.join(JOURNAL_FILE), &bytes).unwrap();
            assert!(
                matches!(TaskStore::open(&crashed.0), Err(error) if error.kind() == io::ErrorKind::InvalidData)
            );
            assert_eq!(fs::read(crashed.0.join(JOURNAL_FILE)).unwrap(), bytes);
            assert_eq!(fs::read(crashed.0.join(STATE_FILE)).unwrap(), snapshot);
        }
    }

    #[test]
    fn a_task_cannot_be_moved_to_another_thread() {
        // Other tasks name a thread by its root; moving the root would
        // orphan them, so `update` refuses.
        let store = TaskStore::memory();
        let first = store.create(intent()).unwrap();
        let second = store.create(intent()).unwrap();
        store.create_in(intent(), Some(first.id)).unwrap();
        assert!(store
            .update(first.id, |task| task.thread = second.id)
            .is_err());
        assert!(store
            .update(second.id, |task| task.thread = first.id)
            .is_err());
    }

    #[test]
    fn persists_terminal_tasks_and_allocates_ids_across_restarts() {
        let dir = TempDir::new();
        let store = TaskStore::open(&dir.0).unwrap();
        let first = store.create(intent()).unwrap();
        assert_eq!(first.id, TaskId(1));
        assert_eq!(first.intent.id, IntentId(1));
        store
            .update(first.id, |task| task.state = TaskState::Succeeded)
            .unwrap();
        drop(store);
        let store = TaskStore::open(&dir.0).unwrap();
        assert_eq!(store.get(first.id).unwrap().state, TaskState::Succeeded);
        assert_eq!(store.create(intent()).unwrap().id, TaskId(2));
        assert_eq!(
            store.list().iter().map(|task| task.id).collect::<Vec<_>>(),
            vec![TaskId(1), TaskId(2)]
        );
    }

    #[test]
    fn restart_interrupts_every_unfinished_state_without_replaying() {
        let dir = TempDir::new();
        let store = TaskStore::open(&dir.0).unwrap();
        for state in [
            TaskState::Queued,
            TaskState::Planning,
            TaskState::Running,
            TaskState::AwaitingConfirmation,
        ] {
            let task = store.create(intent()).unwrap();
            store
                .update(task.id, |task| {
                    task.state = state;
                    task.steps = vec![step(if state == TaskState::Running {
                        StepState::Running
                    } else {
                        StepState::Pending
                    })];
                    if state == TaskState::AwaitingConfirmation {
                        task.confirmation = Some(Confirmation {
                            step: 0,
                            expires_at_ms: now_ms() + 60_000,
                            token: "test-confirmation".into(),
                            verify_identity: false,
                        });
                    }
                })
                .unwrap();
        }
        drop(store);
        let store = TaskStore::open(&dir.0).unwrap();
        for task in store.list() {
            assert_eq!(task.state, TaskState::Interrupted);
            assert!(task.confirmation.is_none());
            assert!(task.message.unwrap().contains("不会自动重复"));
            if task.id == TaskId(3) {
                assert_eq!(task.steps[0].state, StepState::Running);
            }
        }
    }

    #[test]
    fn cancellation_preserves_in_flight_step_and_terminal_tasks() {
        let store = TaskStore::memory();
        let task = store.create(intent()).unwrap();
        store
            .update(task.id, |task| {
                task.state = TaskState::Running;
                task.steps = vec![step(StepState::Running), step(StepState::Pending)];
            })
            .unwrap();
        let cancelled = store.cancel(task.id).unwrap();
        assert_eq!(cancelled.state, TaskState::Cancelled);
        assert_eq!(cancelled.steps[0].state, StepState::Running);
        assert!(store.is_cancelled(task.id));
        assert_eq!(
            store.cancel(task.id).unwrap().updated_at_ms,
            cancelled.updated_at_ms
        );
        assert_eq!(
            store.cancel(TaskId(999)).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn failed_persistence_does_not_publish_mutation_or_consume_id() {
        let dir = TempDir::new();
        let store = TaskStore::open(&dir.0).unwrap();
        let task = store.create(intent()).unwrap();
        let previous_revision = store.revision();
        let mut changes = store.subscribe();
        // Neither the journal nor, once an append has failed, the snapshot
        // that replaces it can be written.
        for file in [STATE_FILE, JOURNAL_FILE] {
            fs::remove_file(dir.0.join(file)).unwrap();
            fs::create_dir(dir.0.join(file)).unwrap();
        }
        assert!(store
            .update(task.id, |task| task.state = TaskState::Failed)
            .is_err());
        assert_eq!(store.get(task.id).unwrap().state, TaskState::Queued);
        assert_eq!(store.revision(), previous_revision);
        assert!(matches!(
            changes.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert!(store.create(intent()).is_err());
        assert_eq!(store.list().len(), 1);
        for file in [STATE_FILE, JOURNAL_FILE] {
            fs::remove_dir(dir.0.join(file)).unwrap();
        }
        assert_eq!(store.create(intent()).unwrap().id, TaskId(2));
        // After a failed append the next change goes to a fresh snapshot,
        // never after what may be a partial line.
        let saved: Value =
            serde_json::from_slice(&fs::read(dir.0.join(STATE_FILE)).unwrap()).unwrap();
        assert_eq!(saved["tasks"].as_array().unwrap().len(), 2);
        assert!(fs::read(dir.0.join(JOURNAL_FILE)).unwrap().is_empty());
    }

    #[test]
    fn rejects_invalid_updates_without_mutating_the_task() {
        let store = TaskStore::memory();
        let task = store.create(intent()).unwrap();
        assert!(store.update(task.id, |task| task.id = TaskId(7)).is_err());
        assert!(store
            .update(task.id, |task| task.state = TaskState::AwaitingConfirmation)
            .is_err());
        assert!(store
            .update(task.id, |task| {
                task.state = TaskState::Succeeded;
                task.steps = vec![step(StepState::Pending)];
            })
            .is_err());
        assert_eq!(store.get(task.id).unwrap().state, TaskState::Queued);
    }

    #[test]
    fn an_unhandled_intent_is_a_terminal_outcome_distinct_from_success() {
        let dir = TempDir::new();
        let store = TaskStore::open(&dir.0).unwrap();
        let task = store.create(intent()).unwrap();
        // "Nothing could be done" never carries steps that look like work.
        assert!(store
            .update(task.id, |task| {
                task.state = TaskState::Unhandled;
                task.steps = vec![step(StepState::Succeeded)];
            })
            .is_err());
        let unhandled = store
            .update(task.id, |task| {
                task.state = TaskState::Unhandled;
                task.message = Some("我还不会处理这句话".into());
            })
            .unwrap();
        assert!(unhandled.state.is_terminal());
        assert_eq!(
            serde_json::to_value(unhandled.state).unwrap(),
            json!("unhandled")
        );
        // Terminal: neither cancellation nor a restart rewrites the outcome.
        assert_eq!(store.cancel(task.id).unwrap().state, TaskState::Unhandled);
        let (page, _, _) = store.page(None, 10, Some(TaskState::Succeeded));
        assert!(page.is_empty());
        drop(store);
        let store = TaskStore::open(&dir.0).unwrap();
        let reloaded = store.get(task.id).unwrap();
        assert_eq!(reloaded.state, TaskState::Unhandled);
        assert_eq!(reloaded.message.as_deref(), Some("我还不会处理这句话"));
    }

    #[test]
    fn rejects_corrupted_or_inconsistent_snapshots_without_overwriting() {
        let dir = TempDir::new();
        let store = TaskStore::open(&dir.0).unwrap();
        store.create(intent()).unwrap();
        drop(store);
        let path = dir.0.join(STATE_FILE);
        let valid: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut cases = vec![b"{partial".to_vec()];
        for (pointer, value) in [
            ("/schema_version", json!(999)),
            ("/next_id", json!(1)),
            ("/revision", json!(0)),
            ("/tasks/0/intent/id", json!(999)),
            ("/tasks/0/state", json!("awaiting_confirmation")),
        ] {
            let mut broken = valid.clone();
            *broken.pointer_mut(pointer).unwrap() = value;
            cases.push(serde_json::to_vec(&broken).unwrap());
        }
        let mut duplicate = valid.clone();
        duplicate["tasks"]
            .as_array_mut()
            .unwrap()
            .push(valid["tasks"][0].clone());
        cases.push(serde_json::to_vec(&duplicate).unwrap());
        for bytes in cases {
            fs::write(&path, &bytes).unwrap();
            assert!(
                matches!(TaskStore::open(&dir.0), Err(error) if error.kind() == io::ErrorKind::InvalidData)
            );
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[cfg(unix)]
    #[test]
    fn directory_lock_is_exclusive_and_released_on_drop() {
        let dir = TempDir::new();
        let store = TaskStore::open(&dir.0).unwrap();
        assert!(
            matches!(TaskStore::open(&dir.0), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        drop(store);
        assert!(TaskStore::open(&dir.0).is_ok());
    }

    #[test]
    fn pages_are_descending_filtered_and_capture_their_revision() {
        let store = TaskStore::memory();
        for _ in 0..5 {
            store.create(intent()).unwrap();
        }
        for id in [TaskId(2), TaskId(4)] {
            store
                .update(id, |task| task.state = TaskState::Succeeded)
                .unwrap();
        }
        let (tasks, next, revision) = store.page(None, 2, None);
        assert_eq!(
            tasks.iter().map(|task| task.id).collect::<Vec<_>>(),
            vec![TaskId(5), TaskId(4)]
        );
        assert_eq!(next, Some(TaskId(4)));
        assert_eq!(revision, store.revision());
        store.create(intent()).unwrap();
        let (tasks, next, _) = store.page(next, 2, None);
        assert_eq!(
            tasks.iter().map(|task| task.id).collect::<Vec<_>>(),
            vec![TaskId(3), TaskId(2)]
        );
        assert_eq!(next, Some(TaskId(2)));
        let (tasks, next, _) = store.page(next, 2, None);
        assert_eq!(
            tasks.iter().map(|task| task.id).collect::<Vec<_>>(),
            vec![TaskId(1)]
        );
        assert_eq!(next, None);
        let (tasks, next, _) = store.page(None, 1, Some(TaskState::Succeeded));
        assert_eq!(tasks[0].id, TaskId(4));
        assert_eq!(next, Some(TaskId(4)));
        let (tasks, next, _) = store.page(next, 1, Some(TaskState::Succeeded));
        assert_eq!(tasks[0].id, TaskId(2));
        assert_eq!(next, None);
    }

    #[test]
    fn subscribers_observe_committed_revisions_and_terminal_cancellation_is_a_no_op() {
        let store = TaskStore::memory();
        let mut changes = store.subscribe();
        let task = store.create(intent()).unwrap();
        assert_eq!(
            changes.try_recv().unwrap(),
            TaskChange {
                revision: 1,
                id: task.id
            }
        );
        assert!(store.update(task.id, |task| task.id = TaskId(99)).is_err());
        assert_eq!(store.revision(), 1);
        assert!(matches!(
            changes.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        store.cancel(task.id).unwrap();
        assert_eq!(
            changes.try_recv().unwrap(),
            TaskChange {
                revision: 2,
                id: task.id
            }
        );
        store.cancel(task.id).unwrap();
        assert_eq!(store.revision(), 2);
        assert!(matches!(
            changes.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn bounded_subscriptions_signal_lag_instead_of_allocating_without_limit() {
        let store = TaskStore::memory();
        let mut changes = store.subscribe();
        for _ in 0..257 {
            store.create(intent()).unwrap();
        }
        assert!(matches!(
            changes.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(1))
        ));
        assert_eq!(changes.try_recv().unwrap().revision, 2);
    }

    #[test]
    fn legacy_snapshot_migrates_and_recovery_advances_a_durable_revision() {
        let dir = TempDir::new();
        let store = TaskStore::open(&dir.0).unwrap();
        let task = store.create(intent()).unwrap();
        store
            .update(task.id, |task| {
                task.state = TaskState::AwaitingConfirmation;
                task.steps = vec![step(StepState::Pending)];
                task.confirmation = Some(Confirmation {
                    step: 0,
                    expires_at_ms: now_ms() + 60_000,
                    token: "old".into(),
                    verify_identity: false,
                });
            })
            .unwrap();
        drop(store);
        let path = dir.0.join(STATE_FILE);
        let mut snapshot: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        snapshot["schema_version"] = serde_json::json!(1);
        snapshot.as_object_mut().unwrap().remove("revision");
        // Snapshots written before threads existed have no such field.
        snapshot["tasks"][0]
            .as_object_mut()
            .unwrap()
            .remove("thread");
        snapshot["tasks"][0]["confirmation"]
            .as_object_mut()
            .unwrap()
            .remove("token");
        fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let store = TaskStore::open(&dir.0).unwrap();
        let recovered = store.get(task.id).unwrap();
        assert_eq!(recovered.state, TaskState::Interrupted);
        assert!(recovered.confirmation.is_none());
        assert_eq!(recovered.thread, task.id);
        let revision = store.revision();
        assert_eq!(revision, 2);
        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["schema_version"], SCHEMA_VERSION);
        assert_eq!(saved["revision"], revision);
        drop(store);
        let store = TaskStore::open(&dir.0).unwrap();
        assert_eq!(store.revision(), revision);
        assert_eq!(store.create(intent()).unwrap().id, TaskId(2));
        assert_eq!(store.revision(), revision + 1);
    }

    #[test]
    fn a_task_continues_only_an_existing_thread_of_its_own_source() {
        let store = TaskStore::memory();
        let first = store.create(intent()).unwrap();
        assert_eq!(first.thread, first.id);
        let second = store.create_in(intent(), Some(first.id)).unwrap();
        let third = store.create_in(intent(), Some(first.id)).unwrap();
        assert_eq!((second.thread, third.thread), (first.id, first.id));
        let other = store.create(intent()).unwrap();
        assert_eq!(other.thread, other.id);
        assert_eq!(
            store
                .earlier_in_thread(third.id, 8)
                .iter()
                .map(|task| task.id)
                .collect::<Vec<_>>(),
            vec![first.id, second.id]
        );
        assert_eq!(store.earlier_in_thread(third.id, 1)[0].id, second.id);
        // A thread is named by its first task: a later task of it, or a task
        // that does not exist, names no thread.
        for missing in [second.id, TaskId(99)] {
            assert_eq!(
                store.create_in(intent(), Some(missing)).unwrap_err().kind(),
                io::ErrorKind::NotFound
            );
        }
        // Guards "conversations never mix": another source cannot join.
        let stranger = Intent {
            source: DeviceId::new("elsewhere"),
            ..intent()
        };
        assert_eq!(
            store
                .create_in(stranger, Some(first.id))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(store.list().len(), 4);
    }

    #[test]
    fn a_question_exists_exactly_while_a_task_awaits_input_and_never_survives_it() {
        let dir = TempDir::new();
        let store = TaskStore::open(&dir.0).unwrap();
        let ask = |task: &mut Task| {
            task.state = TaskState::AwaitingInput;
            task.steps = vec![step(StepState::Pending)];
            task.question = Some(Question {
                step: 0,
                text: "哪一天？".into(),
            });
        };
        let waiting = store.create(intent()).unwrap();
        store.update(waiting.id, ask).unwrap();
        // Waiting without a question, or a question without waiting, is not
        // a state a client could act on.
        assert!(store
            .update(waiting.id, |task| task.question = None)
            .is_err());
        assert!(store
            .update(waiting.id, |task| task.state = TaskState::Running)
            .is_err());
        assert!(store.get(waiting.id).unwrap().question.is_some());
        let cancelled = store.create(intent()).unwrap();
        store.update(cancelled.id, ask).unwrap();
        let cancelled = store.cancel(cancelled.id).unwrap();
        assert_eq!(cancelled.state, TaskState::Cancelled);
        assert!(cancelled.question.is_none());
        drop(store);
        // Guards "a restart interrupts": the planner conversation is gone,
        // so nothing may still look answerable.
        let store = TaskStore::open(&dir.0).unwrap();
        let recovered = store.get(waiting.id).unwrap();
        assert_eq!(recovered.state, TaskState::Interrupted);
        assert!(recovered.question.is_none());
    }

    #[test]
    fn a_new_state_directory_and_its_missing_parents_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let nested = dir.0.join("state").join("agentos");
        let store = TaskStore::open(&nested).unwrap();
        // Guards the control socket's parent check: whatever the umask, the
        // directory the service creates is one it will accept.
        for created in [&nested, &dir.0.join("state")] {
            let mode = fs::metadata(created).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", created.display());
        }
        drop(store);
        // An existing directory is left as it is.
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o750)).unwrap();
        drop(TaskStore::open(&nested).unwrap());
        assert_eq!(
            fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
            0o750
        );
    }
}
