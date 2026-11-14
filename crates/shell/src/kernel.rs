//! Driving the desktop from the task service (`agentos-taskd`) over protocol v1.
//!
//! The page speaks a small event vocabulary (`plan`, `step`, `confirm`,
//! `expression`, `done`, ...). The task service does not push those: it owns
//! durable tasks and announces that one changed. This module watches the
//! service, reads each changed task, and turns the difference from what the
//! page last saw into the page's events; answers from the page become
//! protocol requests. Nothing here decides anything: approval is still bound
//! to the task, step and token the service issued, and the PIN is only passed
//! through.
//!
//! Voice works the same way: the talk button becomes `listen` requests, the
//! service records and recognizes, and the words of a recording this window
//! started come back here and are submitted like typed text, marked spoken.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use agentos_cli::client::{Client, ClientError};
use agentos_core::{CapabilitySpec, RiskLevel};
use agentos_ipc::{Command, EventKind, ListenAction, NoticeLevel, Reply, SpeakMode};
use agentos_runtime::{StepState, Task, TaskId, TaskState};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// What the page may ask the task service for.
#[derive(Debug, PartialEq)]
pub enum Request {
    Intent(String),
    /// Answer the pending approval of `task` (the newest waiting one if
    /// absent), with the PIN the page collected for an irreversible step.
    Confirm {
        approved: bool,
        task: Option<u64>,
        pin: Option<String>,
    },
    /// The talk button: pressed, released, or released elsewhere.
    Listen(ListenAction),
    /// When replies are read aloud, or stop reading now.
    Speech {
        mode: Option<SpeakMode>,
        stop: bool,
    },
}

fn risk_label(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::ReadOnly => "L0",
        RiskLevel::Reversible => "L1",
        RiskLevel::External => "L2",
        RiskLevel::Irreversible => "L3",
    }
}

fn step_status(state: StepState) -> &'static str {
    match state {
        StepState::Pending => "pending",
        StepState::Running => "running",
        StepState::Succeeded => "done",
        StepState::Failed => "failed",
        StepState::Skipped => "cancelled",
    }
}

fn outcome(state: TaskState) -> &'static str {
    match state {
        TaskState::Succeeded => "done",
        TaskState::Unhandled => "unhandled",
        TaskState::Cancelled => "cancelled",
        _ => "failed",
    }
}

/// What a step's result says, for a chat bubble.
fn result_text(result: &Value) -> String {
    result
        .get("message")
        .and_then(Value::as_str)
        .or(result.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| result.to_string())
}

/// The page's events for one task, given what it last showed of that task.
/// `specs` supplies descriptions and risk levels for the plan card.
pub fn translate(
    previous: Option<&Task>,
    task: &Task,
    specs: &HashMap<String, CapabilitySpec>,
) -> Vec<Value> {
    let mut events = Vec::new();
    let (id, plan) = (task.id.0, task.id.to_string());
    let ids = |t: &Task| t.steps.iter().map(|s| s.id.clone()).collect::<Vec<_>>();
    // The card shows every step of the task. Steps are added round by round
    // (and a delegate's are inserted after the step that delegated), so the
    // card is redrawn whenever the list itself changed.
    let redrawn = previous.is_none_or(|p| ids(p) != ids(task));
    if redrawn && !task.steps.is_empty() {
        let steps: Vec<Value> = task
            .steps
            .iter()
            .map(|step| {
                let spec = specs.get(step.capability.as_str());
                json!({
                    "capability": step.capability,
                    "description": spec.map(|s| s.description.as_str()),
                    "risk": spec.map(|s| risk_label(s.risk)),
                    "agent": step.agent,
                })
            })
            .collect();
        events.push(json!({
            "type": "plan", "plan": plan, "intent": id, "task": id,
            "agent": "butler", "steps": steps,
        }));
    }
    let waiting = task.confirmation.as_ref().map(|c| c.step);
    for (index, step) in task.steps.iter().enumerate() {
        let before = previous
            .and_then(|p| p.steps.iter().find(|s| s.id == step.id))
            .map(|s| s.state);
        let status = if waiting == Some(index) {
            "confirm"
        } else {
            step_status(step.state)
        };
        let was_waiting = previous
            .and_then(|p| p.confirmation.as_ref().map(|c| p.steps.get(c.step)))
            .flatten()
            .is_some_and(|s| s.id == step.id);
        let changed = before != Some(step.state) || (waiting == Some(index)) != was_waiting;
        if (changed || redrawn) && status != "pending" {
            events.push(json!({
                "type": "step", "plan": plan, "intent": id, "task": id,
                "index": index, "status": status,
            }));
        }
        // A step that just settled says what it did. The answer to a question
        // is the user's own words, which the page already shows.
        if before != Some(step.state) && step.capability.as_str() != "ask.user" {
            match (step.state, &step.result) {
                (StepState::Succeeded, Some(result)) => events.push(json!({
                    "type": "expression", "kind": "report", "intent": id,
                    "from": step.agent.as_ref().map_or("butler", |a| a.as_str()),
                    "text": result_text(result),
                })),
                (StepState::Failed, Some(result)) => events.push(json!({
                    "type": "expression", "kind": "error", "intent": id,
                    "from": step.agent.as_ref().map_or("butler", |a| a.as_str()),
                    "text": format!("{} 失败：{}", step.capability, result_text(result)),
                })),
                _ => {}
            }
        }
    }
    // A new approval request: the exact step, arguments and reason.
    if let Some(confirmation) = &task.confirmation {
        let asked_before = previous
            .and_then(|p| p.confirmation.as_ref())
            .is_some_and(|c| c.token == confirmation.token);
        if let (false, Some(step)) = (asked_before, task.steps.get(confirmation.step)) {
            let spec = specs.get(step.capability.as_str());
            events.push(json!({
                "type": "confirm", "intent": id, "task": id, "plan": plan,
                "index": confirmation.step,
                "agent": step.agent.as_ref().map_or("butler", |a| a.as_str()),
                "agent_name": step.agent,
                "capability": step.capability,
                "description": spec.map(|s| s.description.as_str()),
                "risk": spec.map_or("L2", |s| risk_label(s.risk)),
                "reason": task.message,
                "verify_identity": confirmation.verify_identity,
                "pin_required": confirmation.verify_identity,
                "args": step.args,
            }));
        }
    }
    // A new question: the page has no separate control for it, so the bridge
    // says how it will be answered.
    if let Some(question) = &task.question {
        let asked_before = previous.is_some_and(|p| p.question.as_ref() == Some(question));
        if !asked_before {
            let from = task
                .steps
                .get(question.step)
                .and_then(|step| step.agent.as_ref())
                .map_or("butler", |agent| agent.as_str());
            events.push(json!({
                "type": "expression", "kind": "report", "intent": id, "task": id, "from": from,
                "text": format!("{}\n（你输入的下一句话会作为对这个问题的回答。）", question.text),
            }));
        }
    }
    // The task ended: say how, unless a step already said exactly that.
    if task.state.is_terminal() && previous.is_none_or(|p| !p.state.is_terminal()) {
        let said: Vec<String> = task
            .steps
            .iter()
            .filter_map(|s| s.result.as_ref().map(result_text))
            .collect();
        if let Some(message) = task
            .message
            .as_ref()
            .filter(|m| m.as_str() != "任务已完成" && !said.iter().any(|text| text == m.as_str()))
        {
            let kind = match task.state {
                TaskState::Succeeded | TaskState::Unhandled => "report",
                _ => "error",
            };
            events.push(json!({
                "type": "expression", "kind": kind, "intent": id, "from": "butler",
                "text": message,
            }));
        }
        events.push(json!({
            "type": "done", "intent": id, "task": id, "state": outcome(task.state),
        }));
    }
    events
}

struct Session {
    client: Client,
    specs: HashMap<String, CapabilitySpec>,
    /// What the page last saw of each task.
    shown: BTreeMap<u64, Task>,
    /// The conversation of this window: one run of the shell is one thread.
    thread: Option<TaskId>,
    /// Recordings this window started; only their words are its to submit.
    recordings: HashSet<u64>,
}

impl Session {
    async fn task(&self, id: u64) -> Result<Task, ClientError> {
        match self.client.request(Command::Get { id: TaskId(id) }).await? {
            Reply::Task { task } => Ok(task),
            _ => unreachable!("the client verifies reply types"),
        }
    }

    /// Read a task and report what changed since the page last saw it.
    async fn refresh(&mut self, id: u64, emit: &impl Fn(Value) -> bool) -> Result<(), ClientError> {
        let task = self.task(id).await?;
        let ended = task.state.is_terminal()
            && self
                .shown
                .get(&id)
                .is_none_or(|before| !before.state.is_terminal());
        for event in translate(self.shown.get(&id), &task, &self.specs) {
            emit(event);
        }
        self.shown.insert(id, task);
        // The panel is the audit log's view; bring it up to date once a task
        // is over rather than on every step.
        if ended {
            self.activity(emit).await?;
        }
        Ok(())
    }

    /// Recent activity as the audit log records it.
    async fn activity(&self, emit: &impl Fn(Value) -> bool) -> Result<(), ClientError> {
        let Reply::Activity { items } =
            self.client.request(Command::Activity { limit: 20 }).await?
        else {
            unreachable!("the client verifies reply types")
        };
        emit(json!({"type": "activity", "items": items}));
        Ok(())
    }

    /// The page's view of the service's voice event, if it has one.
    async fn voice_event(&mut self, kind: EventKind, emit: &impl Fn(Value) -> bool) {
        match kind {
            EventKind::VoiceState {
                mic,
                speaking,
                speak,
            } => {
                emit(json!({"type": "voice", "mic": mic, "speaking": speaking, "speak": speak}));
            }
            EventKind::VoiceLevel { level } => {
                emit(json!({"type": "voice_level", "level": level}));
            }
            EventKind::VoiceNotice { level, text } => {
                emit(json!({"type": "voice_notice", "level": level, "text": text}));
            }
            // Another window's recording is that window's to submit.
            EventKind::Transcript { recording, text } if self.recordings.remove(&recording) => {
                emit(json!({"type": "transcript", "text": text}));
                if let Err(error) = self.say(text, true, emit).await {
                    emit(json!({"type": "expression", "kind": "error", "text": error.to_string()}));
                    emit(json!({"type": "done"}));
                }
            }
            _ => {}
        }
    }

    /// After (re)connecting: learn the current tasks without replaying their
    /// history as chat, but do surface approvals that are still waiting.
    async fn resync(&mut self, emit: &impl Fn(Value) -> bool) -> Result<(), ClientError> {
        let Reply::Capabilities { capabilities } =
            self.client.request(Command::Capabilities {}).await?
        else {
            unreachable!("the client verifies reply types")
        };
        self.specs = capabilities
            .into_iter()
            .map(|spec| (spec.name.to_string(), spec))
            .collect();
        let Reply::Tasks { tasks, .. } = self
            .client
            .request(Command::List {
                before: None,
                limit: 20,
                state: None,
                thread: None,
            })
            .await?
        else {
            unreachable!("the client verifies reply types")
        };
        for summary in tasks.iter().rev() {
            let id = summary.id.0;
            let task = self.task(id).await?;
            let known = self.shown.contains_key(&id);
            if known
                || matches!(
                    task.state,
                    TaskState::AwaitingConfirmation | TaskState::AwaitingInput
                )
            {
                for event in translate(self.shown.get(&id), &task, &self.specs) {
                    emit(event);
                }
            }
            self.shown.insert(id, task);
        }
        self.activity(emit).await
    }

    /// Words from the human, typed or spoken. The page told them that their
    /// next words answer the newest open question; they go to exactly that
    /// task and step, and nowhere else.
    async fn say(
        &mut self,
        text: String,
        spoken: bool,
        emit: &impl Fn(Value) -> bool,
    ) -> Result<(), ClientError> {
        let asked = self.shown.values().rev().find_map(|task| {
            let question = task.question.as_ref()?;
            (task.state == TaskState::AwaitingInput).then_some((task.id, question.step))
        });
        let command = match asked {
            Some((id, step)) => Command::Reply {
                id,
                step: step + 1,
                text,
                spoken,
            },
            None => Command::Submit {
                text,
                thread: self.thread,
                spoken,
            },
        };
        match self.client.request(command).await? {
            Reply::Accepted { task } => {
                self.thread = Some(task.thread);
                self.shown.insert(task.id.0, task);
                Ok(())
            }
            Reply::ReplyQueued { id, .. } => self.refresh(id.0, emit).await,
            _ => Ok(()),
        }
    }

    /// The talk button. A refused start says why and releases the button.
    async fn listen(
        &mut self,
        action: ListenAction,
        emit: &impl Fn(Value) -> bool,
    ) -> Result<(), ClientError> {
        match self.client.request(Command::Listen { action }).await {
            Ok(Reply::Listening { recording }) => {
                if action == ListenAction::Start {
                    self.recordings.insert(recording);
                }
                Ok(())
            }
            Ok(_) => Ok(()),
            // Released after a refused start: nothing to stop.
            Err(ClientError::Remote(error)) if error.code == "not_listening" => Ok(()),
            Err(ClientError::Remote(error)) => {
                emit(
                    json!({"type": "voice_notice", "level": NoticeLevel::Error, "text": error.message}),
                );
                emit(json!({"type": "voice", "mic": "idle", "speaking": false}));
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn handle(&mut self, request: Request, emit: &impl Fn(Value) -> bool) {
        let result = match request {
            Request::Intent(text) => self.say(text, false, emit).await,
            Request::Listen(action) => self.listen(action, emit).await,
            Request::Speech { mode, stop } => {
                match self.client.request(Command::Speech { mode, stop }).await {
                    Ok(Reply::Voice { voice }) => {
                        emit(
                            json!({"type": "voice", "mic": voice.mic, "speaking": voice.speaking, "speak": voice.speak}),
                        );
                        Ok(())
                    }
                    Ok(_) => Ok(()),
                    Err(ClientError::Remote(error)) => {
                        emit(
                            json!({"type": "voice_notice", "level": NoticeLevel::Error, "text": error.message}),
                        );
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
            Request::Confirm {
                approved,
                task,
                pin,
            } => self.confirm(approved, task, pin, emit).await,
        };
        if let Err(error) = result {
            emit(json!({"type": "expression", "kind": "error", "text": error.to_string()}));
            emit(json!({"type": "done"}));
        }
    }

    async fn confirm(
        &mut self,
        approved: bool,
        task: Option<u64>,
        pin: Option<String>,
        emit: &impl Fn(Value) -> bool,
    ) -> Result<(), ClientError> {
        let waiting = |t: &&Task| t.state == TaskState::AwaitingConfirmation;
        let Some(id) = task
            .filter(|id| self.shown.get(id).is_some_and(|t| waiting(&t)))
            .or_else(|| self.shown.values().rev().find(waiting).map(|t| t.id.0))
        else {
            return Ok(());
        };
        if !approved {
            self.client
                .request(Command::Cancel { id: TaskId(id) })
                .await?;
            return self.refresh(id, emit).await;
        }
        // Approve exactly what the page showed: the step and token read with it.
        let Some(confirmation) = self.shown.get(&id).and_then(|t| t.confirmation.clone()) else {
            return Ok(());
        };
        let command = Command::Approve {
            id: TaskId(id),
            step: confirmation.step + 1,
            confirmation: confirmation.token,
            pin,
        };
        match self.client.request(command).await {
            Ok(_) => Ok(()),
            // A wrong or missing PIN leaves the step waiting: ask again.
            Err(ClientError::Remote(error))
                if matches!(
                    error.code.as_str(),
                    "identity_required" | "identity_rejected" | "identity_unavailable"
                ) =>
            {
                emit(json!({"type": "pin", "ok": false, "task": id, "message": error.message}));
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

/// Keep the page in step with the task service until the window closes.
/// `emit` returns `false` once the page is gone.
pub fn run(
    socket: PathBuf,
    mut requests: mpsc::UnboundedReceiver<Request>,
    emit: impl Fn(Value) -> bool,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("agentos-shell：无法启动任务服务连接：{error}");
            return;
        }
    };
    runtime.block_on(async move {
        let mut session = Session {
            client: Client::new(socket),
            specs: HashMap::new(),
            shown: BTreeMap::new(),
            thread: None,
            recordings: HashSet::new(),
        };
        loop {
            let connected = async {
                let Reply::Pong { server_version, .. } =
                    session.client.request(Command::Ping {}).await?
                else {
                    unreachable!("the client verifies reply types")
                };
                // Subscribe before reading, so no change falls in between.
                let stream = session.client.watch_with(None, true).await?;
                let Reply::Voice { voice } = session.client.request(Command::Voice {}).await? else {
                    unreachable!("the client verifies reply types")
                };
                // Recordings of a previous connection will not be answered.
                session.recordings.clear();
                emit(json!({
                    "type": "hello", "planner": "任务服务", "version": server_version,
                    "voice": voice,
                }));
                session.resync(&emit).await?;
                Ok::<_, ClientError>(stream)
            }
            .await;
            if let Ok(mut stream) = connected {
                // A dedicated task owns the stream: a frame is never half-read.
                let (changes_tx, mut changes) = mpsc::unbounded_channel();
                let reader = tokio::spawn(async move {
                    while let Ok(Some(event)) = stream.next().await {
                        if changes_tx.send(event.kind).is_err() {
                            break;
                        }
                    }
                });
                loop {
                    let step = tokio::select! {
                        change = changes.recv() => match change {
                            Some(EventKind::TaskChanged { id }) => session.refresh(id.0, &emit).await,
                            Some(EventKind::ResyncRequired { .. }) => session.resync(&emit).await,
                            Some(voice) => { session.voice_event(voice, &emit).await; Ok(()) }
                            None => break,
                        },
                        request = requests.recv() => match request {
                            Some(request) => { session.handle(request, &emit).await; Ok(()) }
                            None => { reader.abort(); return; }
                        },
                    };
                    if step.is_err() {
                        break;
                    }
                }
                reader.abort();
            }
            if !emit(json!({"type": "connection", "connected": false})) {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::{now_ms, AgentId, CapabilityName, DeviceId, Intent, IntentId, Urgency};
    use agentos_runtime::{Confirmation, TaskStep, TaskStore};

    fn specs() -> HashMap<String, CapabilitySpec> {
        [
            ("notes.append", "追加一条笔记", RiskLevel::Reversible),
            ("disk.wipe", "清除磁盘", RiskLevel::Irreversible),
        ]
        .into_iter()
        .map(|(name, description, risk)| {
            (
                name.to_string(),
                CapabilitySpec {
                    name: CapabilityName::new(name),
                    description: description.into(),
                    risk,
                    input_schema: json!({}),
                },
            )
        })
        .collect()
    }

    fn step(id: &str, capability: &str, state: StepState, result: Option<Value>) -> TaskStep {
        TaskStep {
            id: id.into(),
            state,
            result,
            ..TaskStep::pending(CapabilityName::new(capability), json!({"text": "牛奶"}))
        }
    }

    struct Fixture(TaskStore, TaskId);

    impl Fixture {
        fn new(text: &str) -> (Self, Task) {
            let store = TaskStore::memory();
            let task = store
                .create(Intent {
                    id: IntentId(0),
                    source: DeviceId::new("test"),
                    speaker: None,
                    text: text.into(),
                    urgency: Urgency::Normal,
                    at_ms: now_ms(),
                })
                .unwrap();
            (Self(store, task.id), task)
        }

        fn update(&self, change: impl FnOnce(&mut Task)) -> Task {
            self.0.update(self.1, change).unwrap()
        }
    }

    fn kinds(events: &[Value]) -> Vec<String> {
        events
            .iter()
            .map(|e| match e["type"].as_str().unwrap() {
                "step" => format!("step:{}:{}", e["index"], e["status"].as_str().unwrap()),
                "expression" => format!(
                    "{}:{}",
                    e["kind"].as_str().unwrap(),
                    e["text"].as_str().unwrap()
                ),
                "done" => format!("done:{}", e["state"].as_str().unwrap()),
                other => other.to_string(),
            })
            .collect()
    }

    #[test]
    fn a_task_is_shown_as_a_plan_then_step_results_then_its_outcome() {
        let (fixture, queued) = Fixture::new("记一下 牛奶");
        assert!(translate(None, &queued, &specs()).is_empty());
        let planned = fixture.update(|t| {
            t.state = TaskState::Running;
            t.steps = vec![step("call-1", "notes.append", StepState::Pending, None)];
        });
        let events = translate(Some(&queued), &planned, &specs());
        assert_eq!(kinds(&events), vec!["plan"]);
        assert_eq!(events[0]["steps"][0]["description"], "追加一条笔记");
        assert_eq!(events[0]["steps"][0]["risk"], "L1");

        let running = fixture.update(|t| t.steps[0].state = StepState::Running);
        assert_eq!(
            kinds(&translate(Some(&planned), &running, &specs())),
            vec!["step:0:running"]
        );
        let done = fixture.update(|t| {
            t.steps[0].state = StepState::Succeeded;
            t.steps[0].result = Some(json!({"message": "已记下：牛奶"}));
            t.state = TaskState::Succeeded;
            t.message = Some("任务已完成".into());
        });
        assert_eq!(
            kinds(&translate(Some(&running), &done, &specs())),
            vec!["step:0:done", "report:已记下：牛奶", "done:done"]
        );
        // Nothing changed: nothing is said twice.
        assert!(translate(Some(&done), &done, &specs()).is_empty());
    }

    #[test]
    fn not_understood_and_failed_tasks_end_with_their_own_outcome() {
        let (fixture, queued) = Fixture::new("讲个笑话");
        let unhandled = fixture.update(|t| {
            t.state = TaskState::Unhandled;
            t.message = Some("我还不会处理这句话。".into());
        });
        assert_eq!(
            kinds(&translate(Some(&queued), &unhandled, &specs())),
            vec!["report:我还不会处理这句话。", "done:unhandled"]
        );

        let (fixture, queued) = Fixture::new("记一下 牛奶");
        let failed = fixture.update(|t| {
            t.state = TaskState::Failed;
            t.steps = vec![
                step(
                    "call-1",
                    "notes.append",
                    StepState::Failed,
                    Some(json!("磁盘已满")),
                ),
                step("call-2", "notes.append", StepState::Skipped, None),
            ];
            t.message = Some("磁盘已满".into());
        });
        // The step's failure is said once; the task's message repeats it.
        assert_eq!(
            kinds(&translate(Some(&queued), &failed, &specs())),
            vec![
                "plan",
                "step:0:failed",
                "error:notes.append 失败：磁盘已满",
                "step:1:cancelled",
                "done:failed"
            ]
        );
    }

    #[test]
    fn a_question_is_said_once_and_its_answer_is_not_echoed_as_a_result() {
        let (fixture, queued) = Fixture::new("记一下开会的事");
        let asking = fixture.update(|t| {
            t.state = TaskState::AwaitingInput;
            t.steps = vec![step("call-1", "ask.user", StepState::Pending, None)];
            t.message = Some("哪一天开会？".into());
            t.question = Some(agentos_runtime::Question {
                step: 0,
                text: "哪一天开会？".into(),
            });
        });
        let events = translate(Some(&queued), &asking, &specs());
        let said: Vec<&str> = events
            .iter()
            .filter(|e| e["type"] == "expression")
            .map(|e| e["text"].as_str().unwrap())
            .collect();
        assert_eq!(said.len(), 1);
        // The page has no control of its own for answering: it is told how.
        assert!(said[0].starts_with("哪一天开会？") && said[0].contains("下一句话"));
        // Seeing the same question again says nothing new.
        assert!(translate(Some(&asking), &asking, &specs()).is_empty());
        let answered = fixture.update(|t| {
            t.state = TaskState::Running;
            t.question = None;
            t.message = None;
            t.steps[0].state = StepState::Succeeded;
            t.steps[0].result = Some(json!({"answer": "周三"}));
        });
        assert_eq!(
            kinds(&translate(Some(&asking), &answered, &specs())),
            vec!["step:0:done"]
        );
    }

    #[test]
    fn an_approval_request_carries_the_exact_step_and_whether_a_pin_is_needed() {
        let (fixture, queued) = Fixture::new("清除磁盘");
        let waiting = fixture.update(|t| {
            t.state = TaskState::AwaitingConfirmation;
            t.steps = vec![step("call-1", "disk.wipe", StepState::Pending, None)];
            t.message = Some("该操作不可撤销或涉及安全".into());
            t.confirmation = Some(Confirmation {
                step: 0,
                expires_at_ms: now_ms() + 60_000,
                token: "token-1".into(),
                verify_identity: true,
            });
        });
        let events = translate(Some(&queued), &waiting, &specs());
        assert_eq!(kinds(&events), vec!["plan", "step:0:confirm", "confirm"]);
        let confirm = &events[2];
        assert_eq!(confirm["task"], waiting.id.0);
        assert_eq!(confirm["capability"], "disk.wipe");
        assert_eq!(confirm["risk"], "L3");
        assert_eq!(confirm["pin_required"], true);
        assert_eq!(confirm["args"], json!({"text": "牛奶"}));
        // The token never reaches the page: it stays with the bridge.
        assert!(!confirm.to_string().contains("token-1"));
        // The same request is not asked again on an unrelated refresh.
        assert!(translate(Some(&waiting), &waiting, &specs()).is_empty());
    }

    #[test]
    fn a_delegates_steps_redraw_the_card_in_place() {
        let (fixture, queued) = Fixture::new("@notes 记一下 牛奶");
        let delegating = fixture.update(|t| {
            t.state = TaskState::Running;
            t.steps = vec![step("call-1", "agent.notes", StepState::Running, None)];
        });
        translate(Some(&queued), &delegating, &specs());
        let inside = fixture.update(|t| {
            let mut inner = step("call-2", "notes.append", StepState::Pending, None);
            inner.agent = Some(AgentId::new("notes"));
            t.steps.push(inner);
        });
        let events = translate(Some(&delegating), &inside, &specs());
        assert_eq!(kinds(&events), vec!["plan", "step:0:running"]);
        assert_eq!(events[0]["steps"][1]["agent"], "notes");
    }

    /// The whole bridge against a real task service on a real socket.
    /// A task service on its own thread and runtime, like the daemon. With
    /// `words`, its microphone hears a tone and its recognizer those words.
    fn serve(name: &str, words: Option<&str>) -> (std::path::PathBuf, impl FnOnce()) {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::mpsc as std_mpsc;

        let dir = std::path::PathBuf::from(format!(
            "{}/aos-shell-{name}-{}",
            if cfg!(target_os = "macos") {
                "/private/tmp".to_string()
            } else {
                std::env::temp_dir().display().to_string()
            },
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let socket = dir.join("control.sock");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let (ready, is_ready) = std_mpsc::channel();
        let service_socket = socket.clone();
        let audit_path = dir.join("audit.jsonl");
        let words = words.map(str::to_string);
        let service = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    use std::sync::Arc;
                    let audit = Arc::new(agentos_audit::AuditLog::open(audit_path).unwrap());
                    let mut registry = agentos_services::registry::CapabilityRegistry::new();
                    agentos_taskd::builtin::register_all(&mut registry);
                    let mut policy = agentos_policy::PolicyEngine::new();
                    let names: Vec<_> = registry.specs().into_iter().map(|s| s.name).collect();
                    let butler_id = AgentId::new("butler");
                    agentos_taskd::butler::grant_manifest(&mut policy, &butler_id, &names);
                    let butler = agentos_taskd::butler::Butler::with_store(
                        butler_id,
                        registry,
                        policy,
                        Box::new(agentos_planner::RulePlanner),
                        audit.clone(),
                        agentos_services::expression::ExpressionHub::new(audit.clone()),
                        Arc::new(TaskStore::memory()),
                    );
                    let mut service =
                        agentos_taskd::service::Service::with_butler(butler, audit.clone(), None);
                    if let Some(words) = words {
                        use agentos_taskd::voice::{CommandStt, Voice, VoiceConfig};
                        let sh = |script: String| vec!["sh".into(), "-c".into(), script];
                        let config = VoiceConfig {
                            stt: Some(Arc::new(CommandStt::new(sh(format!("echo '{words}'"))))),
                            recorder: sh(
                                "head -c 32000 /dev/zero | tr '\\0' '\\177'; sleep 30".into()
                            ),
                            ..VoiceConfig::disabled()
                        };
                        service = service.with_voice(Voice::with_led(
                            config,
                            audit,
                            agentos_taskd::mic_led::MicLed::disabled(),
                        ));
                    }
                    let server = service.bind(&service_socket).await.unwrap();
                    ready.send(()).unwrap();
                    server
                        .run_until(async {
                            let _ = stopped.await;
                        })
                        .await
                        .unwrap();
                });
        });
        is_ready
            .recv_timeout(Duration::from_secs(10))
            .expect("service ready");
        (socket, move || {
            stop.send(()).unwrap();
            service.join().unwrap();
            let _ = std::fs::remove_dir_all(&dir);
        })
    }

    #[test]
    fn a_held_talk_button_becomes_a_spoken_task_in_this_conversation() {
        use std::sync::mpsc as std_mpsc;
        let (socket, stop) = serve("voice", Some("记一下 说出来的"));
        let (requests, receiver) = mpsc::unbounded_channel();
        let (events_tx, events) = std_mpsc::channel::<Value>();
        let bridge = std::thread::spawn(move || {
            run(socket, receiver, move |event| events_tx.send(event).is_ok())
        });
        let next = |wanted: &str| loop {
            let event = events
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("no {wanted} event"));
            if event["type"] == wanted {
                return event;
            }
        };
        let hello = next("hello");
        assert_eq!(hello["voice"]["input"], true);
        assert_eq!(hello["voice"]["destination"], "本机");
        requests.send(Request::Listen(ListenAction::Start)).unwrap();
        assert_eq!(next("voice")["mic"], "listening");
        std::thread::sleep(Duration::from_millis(400));
        requests.send(Request::Listen(ListenAction::Stop)).unwrap();
        // The page shows what was heard, then the task it became.
        assert_eq!(next("transcript")["text"], "记一下 说出来的");
        let said = next("expression");
        assert_eq!(said["text"], "已记下：说出来的");
        assert_eq!(next("done")["state"], "done");
        // The microphone use is in recent activity, from the audit log.
        let activity = next("activity");
        assert!(activity["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "device"));
        drop(requests);
        bridge.join().unwrap();
        stop();
    }

    #[test]
    fn the_page_drives_a_real_task_service_through_the_bridge() {
        use std::sync::mpsc as std_mpsc;
        let (socket, stop) = serve("page", None);

        let (requests, receiver) = mpsc::unbounded_channel();
        let (events_tx, events) = std_mpsc::channel::<Value>();
        let bridge = std::thread::spawn(move || {
            run(socket, receiver, move |event| events_tx.send(event).is_ok())
        });
        let next = |wanted: &str| loop {
            let event = events
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("no {wanted} event"));
            if event["type"] == wanted {
                return event;
            }
        };
        assert_eq!(next("hello")["voice"]["input"], false);
        next("activity");

        requests
            .send(Request::Intent("记一下 买牛奶".into()))
            .unwrap();
        let plan = next("plan");
        assert_eq!(plan["steps"][0]["capability"], "notes.append");
        assert_eq!(plan["steps"][0]["risk"], "L1");
        let said = next("expression");
        assert_eq!(
            (said["kind"].as_str(), said["text"].as_str()),
            (Some("report"), Some("已记下：买牛奶"))
        );
        assert_eq!(next("done")["state"], "done");

        requests.send(Request::Intent("讲个笑话".into())).unwrap();
        assert_eq!(next("expression")["kind"], "report");
        assert_eq!(next("done")["state"], "unhandled");

        // Closing the page ends the bridge; the service is unaffected.
        drop(requests);
        bridge.join().unwrap();
        stop();
    }
}
