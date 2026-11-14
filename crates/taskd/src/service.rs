//! A bounded, local control service. Connections never own task lifetimes.
//! Admitted tasks run on a bounded pool of workers, independently of each other.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, DirBuilder};
use std::future::Future;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentos_audit::{AuditEvent, AuditLog};
use agentos_core::{
    now_ms, AgentId, CapabilityError, CapabilityName, DeviceId, Intent, IntentId, RiskLevel,
    Urgency,
};
use agentos_ipc::{
    current_uid, read_frame, write_frame, Caller, Command, Event, EventKind, ListenAction,
    MicState, ProtocolError, Reply, Request, Response, SpeakMode, TaskSummary, VoiceStatus,
    MAX_CAPABILITY_NAME_BYTES, MAX_PAGE_SIZE, MAX_TEXT_BYTES, VERSION,
};
use agentos_policy::{PolicyEngine, Subject};
use agentos_runtime::{Task, TaskId, TaskState, TaskStore};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWrite, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::butler::{Butler, CapabilityView};
use crate::control::{approval_matches_with_token, reply_matches};
use crate::preauth::{NewPreauthorization, PreauthStore};
use crate::voice::Voice;
use agentos_planner::Planner;
use agentos_services::expression::ExpressionHub;
use agentos_services::identity::{PinCheck, PinVault};
use agentos_services::registry::CapabilityRegistry;

const MAX_CONNECTIONS: usize = 64;
const MAX_ACTIVITY: usize = 100;
const QUEUE_CAPACITY: usize = 64;
/// Tasks (and approvals being carried out) that run at the same time unless
/// the embedder says otherwise. Each may hold a model request open.
pub const MAX_RUNNING_TASKS: usize = 4;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Service {
    controller: Arc<Controller>,
    work_rx: mpsc::UnboundedReceiver<Admitted>,
    butler: Arc<Butler>,
    max_running: usize,
}

pub struct Server {
    listener: UnixListener,
    socket: SocketGuard,
    service: Service,
}

struct Controller {
    /// For what only the butler knows: what a caller may use right now.
    butler: Arc<Butler>,
    tasks: Arc<TaskStore>,
    audit: Arc<AuditLog>,
    /// What the butler can currently do; it keeps this current as plugins
    /// are installed, disabled or rolled back.
    view: Arc<Mutex<CapabilityView>>,
    /// Proves identity for approvals of irreversible steps.
    pins: Option<Arc<PinVault>>,
    preauths: Arc<PreauthStore>,
    /// The microphone and speaker, when this machine has them configured.
    voice: Option<Arc<Voice>>,
    work_tx: mpsc::UnboundedSender<Admitted>,
    /// Bounds work that has been admitted and has not started. The channel
    /// itself is unbounded because the scheduler takes work off it at once,
    /// to keep the tasks of a thread in order; the bound is this.
    admission: Arc<Semaphore>,
}

enum Work {
    Run(TaskId),
    Approve {
        id: TaskId,
        step: usize,
        responder: Intent,
        token: String,
    },
    Reply {
        id: TaskId,
        step: usize,
        responder: Intent,
        text: String,
    },
}

/// Work with its place in the queue, which it gives up when it starts.
struct Admitted {
    work: Work,
    place: OwnedSemaphorePermit,
}

impl Service {
    /// A service around a plain butler: no agents, blackboard, judge or PIN.
    pub fn new(
        tasks: Arc<TaskStore>,
        audit: Arc<AuditLog>,
        registry: CapabilityRegistry,
        policy: PolicyEngine,
        planner: Box<dyn Planner>,
    ) -> Self {
        let expr = ExpressionHub::new(audit.clone());
        let butler = Butler::with_store(
            AgentId::new("butler"),
            registry,
            policy,
            planner,
            audit.clone(),
            expr,
            tasks,
        );
        Self::with_butler(butler, audit, None)
    }

    /// A service around a butler the caller has configured. `pins` must be
    /// the vault the butler was given, so a PIN checked here vouches for the
    /// approval the butler then carries out.
    pub fn with_butler(butler: Butler, audit: Arc<AuditLog>, pins: Option<Arc<PinVault>>) -> Self {
        let (work_tx, work_rx) = mpsc::unbounded_channel();
        let butler = Arc::new(butler);
        Self {
            controller: Arc::new(Controller {
                butler: butler.clone(),
                tasks: butler.tasks().clone(),
                audit,
                view: butler.view(),
                pins,
                preauths: butler.preauthorizations().clone(),
                voice: None,
                work_tx,
                admission: Arc::new(Semaphore::new(QUEUE_CAPACITY)),
            }),
            work_rx,
            butler,
            max_running: MAX_RUNNING_TASKS,
        }
    }

    /// Serve the microphone and speaker through the protocol. The same hub
    /// must read replies aloud, so it is also an output of the butler's
    /// expression hub (see `voice::SpeechOutput`).
    pub fn with_voice(mut self, voice: Arc<Voice>) -> Self {
        Arc::get_mut(&mut self.controller)
            .expect("the controller is not shared before the service runs")
            .voice = Some(voice);
        self
    }

    /// How many tasks may run at the same time; at least one. Work beyond
    /// that waits in the bounded queue.
    pub fn with_max_running(mut self, max_running: usize) -> Self {
        self.max_running = max_running.max(1);
        self
    }

    /// Bind without starting the worker. Only stale sockets belonging to this
    /// process's OS user may be replaced; ordinary files and symlinks survive.
    pub async fn bind(self, path: impl AsRef<Path>) -> io::Result<Server> {
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(0o700).create(parent)?;
        let parent_meta = fs::metadata(parent)?;
        if parent_meta.uid() != current_uid() || parent_meta.mode() & 0o022 != 0 {
            // Not repaired here: a directory others could write to may
            // already hold files they put there. The person decides.
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "socket parent {} (mode {:o}) must be owned by this user and not writable by group/others; check what is in it, then run chmod 700 on it, or use a private directory",
                    parent.display(),
                    parent_meta.mode() & 0o7777
                ),
            ));
        }
        match fs::symlink_metadata(path) {
            Ok(meta) => {
                if !meta.file_type().is_socket() || meta.uid() != current_uid() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "refusing to replace a non-socket or a socket owned by another user",
                    ));
                }
                match timeout(Duration::from_secs(1), UnixStream::connect(path)).await {
                    Ok(Ok(_)) | Err(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "socket is already in use",
                        ))
                    }
                    Ok(Err(error))
                        if matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                        ) =>
                    {
                        // Recheck the inode before removing the file examined above.
                        match fs::symlink_metadata(path) {
                            Ok(current)
                                if current.dev() == meta.dev() && current.ino() == meta.ino() =>
                            {
                                fs::remove_file(path)?
                            }
                            Ok(_) => {
                                return Err(io::Error::new(
                                    io::ErrorKind::AlreadyExists,
                                    "socket path changed while checking for a stale listener",
                                ))
                            }
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error),
                        }
                    }
                    Ok(Err(error)) => return Err(error),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(path)?;
        let metadata = fs::symlink_metadata(path)?;
        let socket = SocketGuard {
            path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(Server {
            listener,
            socket,
            service: self,
        })
    }
}

impl Server {
    pub fn path(&self) -> &Path {
        &self.socket.path
    }

    /// Stop accepting on shutdown, close clients, and drain already admitted
    /// work for up to five seconds. An unfinished task remains interrupted.
    pub async fn run_until(self, shutdown: impl Future<Output = ()>) -> io::Result<()> {
        let Self {
            listener,
            socket: _socket,
            service,
        } = self;
        let Service {
            controller,
            work_rx,
            butler,
            max_running,
        } = service;
        let mut worker = tokio::spawn(run_workers(work_rx, butler, max_running));
        let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let mut connections = JoinSet::new();
        tokio::pin!(shutdown);
        let mut worker_done = false;
        let outcome = loop {
            tokio::select! {
                _ = &mut shutdown => break Ok(()),
                result = &mut worker => {
                    worker_done = true;
                    break Err(match result {
                        Ok(()) => io::Error::new(io::ErrorKind::BrokenPipe, "task worker stopped unexpectedly"),
                        Err(error) => io::Error::other(format!("task worker failed: {error}")),
                    });
                }
                accepted = listener.accept() => {
                    let (stream, _) = match accepted { Ok(value) => value, Err(error) => break Err(error) };
                    if stream.peer_cred().map(|cred| cred.uid()).ok() != Some(current_uid()) { continue }
                    let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
                    let controller = controller.clone();
                    connections.spawn(async move {
                        let _permit = permit;
                        // Per-client failure only closes that connection.
                        let _ = serve_connection(stream, controller).await;
                    });
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {},
            }
        };
        drop(listener);
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        let tasks = controller.tasks.clone();
        let audit = controller.audit.clone();
        drop(controller);
        if !worker_done {
            match timeout(DRAIN_TIMEOUT, &mut worker).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    return Err(io::Error::other(format!(
                        "task worker failed while stopping: {error}"
                    )))
                }
                Err(_) => {
                    worker.abort();
                    // Aborting the dispatcher drops, and so aborts, every
                    // running task. Current capabilities are asynchronous.
                    // Bound this final cancellation wait too, so a future
                    // blocking plugin cannot keep the control service alive
                    // indefinitely.
                    let _ = timeout(Duration::from_millis(250), &mut worker).await;
                }
            }
        }
        for task in tasks.list() {
            if !task.state.is_terminal() {
                let interrupted = tasks.update(task.id, |task| {
                    if !task.state.is_terminal() {
                        task.state = TaskState::Interrupted;
                        task.confirmation = None;
                        task.question = None;
                        task.message =
                            Some("后台服务已停止；任务未完成，不会自动重复执行。".into());
                    }
                })?;
                let _ = audit.append(AuditEvent::TaskStateChanged {
                    intent: interrupted.intent.id,
                    state: "interrupted".into(),
                    message: interrupted.message,
                });
            }
        }
        outcome
    }
}

/// Run admitted work, at most `max_running` items at a time, until the queue
/// closes and everything admitted has finished.
///
/// Tasks of one thread start in the order they were submitted: a task waits
/// here, holding no worker, while an earlier task of its thread is queued or
/// running. A task that pauses for the human lets the next one start; being
/// approved or answered later resumes it alongside whatever runs then.
async fn run_workers(
    mut work_rx: mpsc::UnboundedReceiver<Admitted>,
    butler: Arc<Butler>,
    max_running: usize,
) {
    let mut ready: VecDeque<(Admitted, Option<TaskId>)> = VecDeque::new();
    // Threads with a task that is ready or running, and what waits behind it.
    let mut threads: HashMap<TaskId, VecDeque<Admitted>> = HashMap::new();
    let mut running: JoinSet<Option<TaskId>> = JoinSet::new();
    let mut open = true;
    loop {
        while running.len() < max_running {
            let Some((admitted, thread)) = ready.pop_front() else {
                break;
            };
            let Admitted { work, place } = admitted;
            // Started: its place in the queue is free again.
            drop(place);
            let butler = butler.clone();
            running.spawn(async move {
                match work {
                    Work::Run(id) => butler.execute(id).await,
                    Work::Approve {
                        id,
                        step,
                        responder,
                        token,
                    } => {
                        butler
                            .approve_with_token(id, step, &responder, &token)
                            .await
                    }
                    Work::Reply {
                        id,
                        step,
                        responder,
                        text,
                    } => butler.reply(id, step, &responder, &text).await,
                }
                thread
            });
        }
        if !open && running.is_empty() && ready.is_empty() {
            break;
        }
        tokio::select! {
            admitted = work_rx.recv(), if open => {
                let Some(admitted) = admitted else {
                    open = false;
                    continue;
                };
                let thread = match &admitted.work {
                    Work::Run(id) => butler.tasks().thread_of(*id),
                    Work::Approve { .. } | Work::Reply { .. } => None,
                };
                match thread {
                    Some(thread) => match threads.get_mut(&thread) {
                        Some(behind) => behind.push_back(admitted),
                        None => {
                            threads.insert(thread, VecDeque::new());
                            ready.push_back((admitted, Some(thread)));
                        }
                    },
                    None => ready.push_back((admitted, None)),
                }
            }
            Some(finished) = running.join_next(), if !running.is_empty() => {
                let thread = match finished {
                    Ok(thread) => thread,
                    // A panic while running a task is a bug in the kernel,
                    // not in the task: it is passed on so the service stops
                    // instead of running on damaged.
                    Err(error) if error.is_panic() => {
                        std::panic::resume_unwind(error.into_panic())
                    }
                    Err(_) => None,
                };
                if let Some(thread) = thread {
                    match threads.get_mut(&thread).and_then(VecDeque::pop_front) {
                        Some(next) => ready.push_back((next, Some(thread))),
                        None => {
                            threads.remove(&thread);
                        }
                    }
                }
            }
        }
    }
}

struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path) {
            if metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
            {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

fn protocol_error(code: &str, message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: code.into(),
        message: message.into(),
    }
}

fn storage_error(error: io::Error) -> ProtocolError {
    let code = if error.kind() == io::ErrorKind::NotFound {
        "not_found"
    } else {
        "storage_unavailable"
    };
    protocol_error(code, error.to_string())
}

fn responder(uid: u32, text: String) -> Intent {
    Intent {
        id: IntentId(0),
        source: DeviceId::new(format!("unix:uid:{uid}")),
        speaker: Some(format!("uid:{uid}")),
        text,
        urgency: Urgency::Normal,
        at_ms: now_ms(),
    }
}

/// Who a request is made as: the user, or the caller it narrowed itself to.
fn actor_intent(uid: u32, actor: Option<&Caller>, text: String) -> Intent {
    match actor {
        Some(caller) => Intent {
            source: DeviceId::new(caller.source()),
            speaker: Some(caller.source()),
            ..responder(uid, text)
        },
        None => responder(uid, text),
    }
}

fn unconfigured_voice() -> VoiceStatus {
    VoiceStatus {
        input: false,
        input_label: None,
        destination: None,
        output: false,
        output_label: None,
        speak: SpeakMode::Off,
        mic: MicState::Idle,
        speaking: false,
    }
}

fn voice_unavailable() -> ProtocolError {
    protocol_error(
        "voice_unavailable",
        "this machine has no voice input or output configured",
    )
}

fn forbidden() -> ProtocolError {
    protocol_error(
        "forbidden",
        "an outside caller cannot do this; it is for the user at this machine",
    )
}

impl Controller {
    /// The caller a request names, as it is registered now.
    fn actor(&self, caller: Option<&str>) -> Result<Option<Caller>, ProtocolError> {
        caller
            .map(|id| {
                self.butler
                    .callers()
                    .get(id)
                    .ok_or_else(|| protocol_error("not_found", format!("no caller {id}")))
            })
            .transpose()
    }

    /// The task, if `actor` may see it. A caller sees the tasks it
    /// submitted; for it, another task does not exist.
    fn task_for(&self, id: TaskId, actor: Option<&Caller>) -> Result<Task, ProtocolError> {
        let task = self.task(id)?;
        match actor {
            Some(caller) if task.intent.source.as_str() != caller.source() => {
                Err(protocol_error("not_found", format!("no task {id}")))
            }
            _ => Ok(task),
        }
    }

    /// The task as `actor` is shown it. The token of a pending approval is
    /// what the human approves with; a caller never gets to hold it.
    fn shown(mut task: Task, actor: Option<&Caller>) -> Task {
        if let (Some(_), Some(confirmation)) = (actor, &mut task.confirmation) {
            confirmation.token.clear();
        }
        task
    }

    fn view(&self) -> std::sync::MutexGuard<'_, CapabilityView> {
        self.view.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn task(&self, id: TaskId) -> Result<Task, ProtocolError> {
        self.tasks
            .get(id)
            .ok_or_else(|| protocol_error("not_found", format!("no task {id}")))
    }

    /// A standing change (a rule, a caller) removes the human from later
    /// decisions, so it takes the strongest proof of who is asking that
    /// exists: the PIN, once one is set. The check vouches for nothing else.
    fn identify(&self, pin: Option<String>) -> Result<(), ProtocolError> {
        let Some(pins) = self.pins.as_ref().filter(|pins| pins.is_set()) else {
            return Ok(());
        };
        let Some(pin) = pin else {
            return Err(protocol_error(
                "identity_required",
                "a PIN is set; this change needs it",
            ));
        };
        match pins.check(&pin) {
            PinCheck::Ok => Ok(()),
            other => Err(protocol_error("identity_rejected", other.message())),
        }
    }

    /// A place in the bounded queue, or a refusal that changed nothing.
    fn admit(&self, refusal: &str) -> Result<OwnedSemaphorePermit, ProtocolError> {
        self.admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| protocol_error("queue_unavailable", refusal))
    }

    fn enqueue(&self, work: Work, place: OwnedSemaphorePermit) -> Result<(), ProtocolError> {
        self.work_tx.send(Admitted { work, place }).map_err(|_| {
            protocol_error(
                "queue_unavailable",
                "the service is stopping; the request was saved but will not run",
            )
        })
    }

    fn handle(
        &self,
        command: Command,
        uid: u32,
        actor: Option<&Caller>,
    ) -> Result<Reply, ProtocolError> {
        // Confirmation, standing permission and who may call at all are the
        // human's: no caller reaches them, whatever its manifest says.
        if actor.is_some()
            && matches!(
                command,
                Command::Approve { .. }
                    | Command::Preauthorize { .. }
                    | Command::Preauthorizations {}
                    | Command::RevokePreauthorization { .. }
                    | Command::RegisterCaller { .. }
                    | Command::Callers {}
                    | Command::RevokeCaller { .. }
                    // The microphone and speaker are the human's at this
                    // machine; no outside program listens or speaks.
                    | Command::Voice {}
                    | Command::Speech { .. }
                    // What the human did, approved and said is theirs.
                    | Command::Activity { .. }
            )
        {
            return Err(forbidden());
        }
        match command {
            Command::Ping {} => Ok(Reply::Pong {
                server_version: env!("CARGO_PKG_VERSION").into(),
                revision: self.tasks.revision(),
            }),
            Command::Submit {
                text,
                thread,
                spoken,
            } => {
                if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
                    return Err(protocol_error(
                        "invalid_argument",
                        "intent must be non-empty and at most 64 KiB",
                    ));
                }
                let place = self.admit("task queue is full or unavailable; no task was created")?;
                let task = self
                    .tasks
                    .create_in(actor_intent(uid, actor, text.trim().into()), thread)
                    .map_err(|error| match (error.kind(), thread) {
                        // Whether a thread is missing or someone else's is
                        // not this client's to learn.
                        (
                            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied,
                            Some(thread),
                        ) => protocol_error(
                            "not_found",
                            format!("no thread {thread}; no task was created"),
                        ),
                        _ => storage_error(error),
                    })?;
                // Marked before the task can run, so its first reply is read.
                if spoken {
                    if let Some(voice) = &self.voice {
                        voice.mark_voice_intent(task.intent.id);
                    }
                }
                self.enqueue(Work::Run(task.id), place)?;
                Ok(Reply::Accepted {
                    task: Self::shown(task, actor),
                })
            }
            Command::List {
                before,
                limit,
                state,
                thread,
            } => {
                if !(1..=MAX_PAGE_SIZE).contains(&limit) {
                    return Err(protocol_error(
                        "invalid_argument",
                        "page limit must be between 1 and 50",
                    ));
                }
                let (tasks, next_before, revision) = self.tasks.page_in(
                    before,
                    limit,
                    state,
                    thread,
                    actor.map(Caller::source).as_deref(),
                );
                Ok(Reply::Tasks {
                    tasks: tasks.iter().map(TaskSummary::from).collect(),
                    next_before,
                    revision,
                })
            }
            Command::Get { id } => Ok(Reply::Task {
                task: Self::shown(self.task_for(id, actor)?, actor),
            }),
            Command::Cancel { id } => {
                self.task_for(id, actor)?;
                let task = self.tasks.cancel(id).map_err(storage_error)?;
                if task.state == TaskState::Cancelled {
                    self.audit
                        .append(AuditEvent::TaskStateChanged {
                            intent: task.intent.id,
                            state: "cancelled".into(),
                            message: task.message.clone(),
                        })
                        .map_err(|error| {
                            protocol_error(
                                "audit_unavailable",
                                format!("cancellation was saved, but audit failed: {error}"),
                            )
                        })?;
                }
                Ok(Reply::Task {
                    task: Self::shown(task, actor),
                })
            }
            Command::Approve {
                id,
                step,
                confirmation,
                pin,
            } => {
                let index = step
                    .checked_sub(1)
                    .ok_or_else(|| protocol_error("invalid_argument", "step numbers start at 1"))?;
                let task = self.task(id)?;
                let responder = responder(uid, format!("approve {id} {step}"));
                if !approval_matches_with_token(&task, index, &responder, now_ms(), &confirmation) {
                    return Err(protocol_error("invalid_confirmation", "approval must match the current task, step, OS user, token and five-minute validity period"));
                }
                // An irreversible step needs proof of who approves it. The
                // PIN is checked here and never leaves this function; a
                // correct one vouches for this task's next approval only.
                if task
                    .confirmation
                    .as_ref()
                    .is_some_and(|c| c.verify_identity)
                {
                    let Some(pins) = &self.pins else {
                        return Err(protocol_error(
                            "identity_unavailable",
                            "this step needs identity verification, which is not configured",
                        ));
                    };
                    let Some(pin) = pin else {
                        return Err(protocol_error(
                            "identity_required",
                            "this step is irreversible; approve it with the PIN",
                        ));
                    };
                    match pins.verify_for(Some(id.0), &pin) {
                        PinCheck::Ok => {}
                        other => return Err(protocol_error("identity_rejected", other.message())),
                    }
                }
                let place =
                    self.admit("approval queue is full or unavailable; retry explicitly")?;
                self.enqueue(
                    Work::Approve {
                        id,
                        step: index,
                        responder,
                        token: confirmation,
                    },
                    place,
                )?;
                Ok(Reply::ApprovalQueued { id, step })
            }
            Command::Reply {
                id,
                step,
                text,
                spoken,
            } => {
                let index = step
                    .checked_sub(1)
                    .ok_or_else(|| protocol_error("invalid_argument", "step numbers start at 1"))?;
                if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
                    return Err(protocol_error(
                        "invalid_argument",
                        "reply must be non-empty and at most 64 KiB",
                    ));
                }
                let task = self.task_for(id, actor)?;
                let responder = actor_intent(uid, actor, format!("reply {id} {step}"));
                if !reply_matches(&task, index, &responder) {
                    return Err(protocol_error(
                        "invalid_reply",
                        "reply must match the question the task is waiting on now, its step and the OS user",
                    ));
                }
                let place = self.admit("reply queue is full or unavailable; retry explicitly")?;
                if spoken {
                    if let Some(voice) = &self.voice {
                        voice.mark_voice_intent(task.intent.id);
                    }
                }
                self.enqueue(
                    Work::Reply {
                        id,
                        step: index,
                        responder,
                        text: text.trim().into(),
                    },
                    place,
                )?;
                Ok(Reply::ReplyQueued { id, step })
            }
            Command::Preauthorize {
                capability,
                caller,
                args,
                other_args,
                expires_in_seconds,
                pin,
            } => {
                // A rule for a caller cannot reach beyond what the caller
                // was given: outside its manifest there is nothing to lift.
                let specs = match &caller {
                    Some(id) => self
                        .butler
                        .caller_specs(id)
                        .ok_or_else(|| protocol_error("not_found", format!("no caller {id}")))?,
                    None => self.view().specs.clone(),
                };
                let risk = specs
                    .iter()
                    .find(|spec| spec.name == capability)
                    .map(|spec| spec.risk)
                    .ok_or_else(|| {
                        protocol_error("not_found", format!("no capability {capability}"))
                    })?;
                // A rule for a call that never asks would do nothing, and an
                // irreversible call is confirmed with identity every time.
                match risk {
                    RiskLevel::Reversible | RiskLevel::External => {}
                    RiskLevel::ReadOnly => {
                        return Err(protocol_error(
                            "not_preauthorizable",
                            format!("{capability} is read-only and never asks for confirmation"),
                        ));
                    }
                    RiskLevel::Irreversible => {
                        return Err(protocol_error(
                            "not_preauthorizable",
                            format!(
                                "{capability} is irreversible; it is confirmed with the PIN every time"
                            ),
                        ));
                    }
                }
                self.identify(pin)?;
                let created = self
                    .preauths
                    .create(
                        NewPreauthorization {
                            subject: caller.map_or(Subject::User, Subject::Caller),
                            capability,
                            args,
                            other_args,
                            expires_in_ms: expires_in_seconds
                                .map(|seconds| seconds.saturating_mul(1_000)),
                        },
                        now_ms(),
                    )
                    .map_err(|error| match error.kind() {
                        io::ErrorKind::InvalidInput => {
                            protocol_error("invalid_argument", error.to_string())
                        }
                        _ => storage_error(error),
                    })?;
                self.audit
                    .append(AuditEvent::PreauthorizationCreated {
                        preauthorization: serde_json::to_value(&created)
                            .expect("a pre-authorization is serializable"),
                        by: format!("uid:{uid}"),
                    })
                    .map_err(|error| {
                        // Never in force without a record of who allowed it.
                        let _ = self.preauths.revoke(&created.id);
                        protocol_error(
                            "audit_unavailable",
                            format!("the pre-authorization was not created: {error}"),
                        )
                    })?;
                Ok(Reply::Preauthorized {
                    preauthorization: created,
                })
            }
            Command::Preauthorizations {} => Ok(Reply::Preauthorizations {
                preauthorizations: self.preauths.active(now_ms()),
            }),
            Command::RevokePreauthorization { id } => {
                let revoked = self
                    .preauths
                    .revoke(&id)
                    .map_err(storage_error)?
                    .ok_or_else(|| {
                        protocol_error("not_found", format!("no pre-authorization {id}"))
                    })?;
                self.audit
                    .append(AuditEvent::PreauthorizationRevoked {
                        id: revoked.id.clone(),
                        by: format!("uid:{uid}"),
                    })
                    .map_err(|error| {
                        protocol_error(
                            "audit_unavailable",
                            format!("the pre-authorization was revoked, but audit failed: {error}"),
                        )
                    })?;
                Ok(Reply::PreauthorizationRevoked { id: revoked.id })
            }
            Command::RegisterCaller {
                id,
                name,
                capabilities,
                max_risk,
                pin,
            } => {
                self.identify(pin)?;
                let known = self.butler.callers().get(&id);
                let registered = self
                    .butler
                    .callers()
                    .put(Caller {
                        // Changing what a caller may use does not make it new.
                        created_at_ms: known.map_or_else(now_ms, |known| known.created_at_ms),
                        id,
                        name: name.trim().into(),
                        capabilities,
                        max_risk,
                    })
                    .map_err(|error| match error.kind() {
                        io::ErrorKind::InvalidInput => {
                            protocol_error("invalid_argument", error.to_string())
                        }
                        _ => storage_error(error),
                    })?;
                // Tasks under way follow the manifest as it is now.
                self.butler.sync_caller(&registered.id);
                self.audit
                    .append(AuditEvent::CallerRegistered {
                        caller: serde_json::to_value(&registered)
                            .expect("a caller is serializable"),
                        by: format!("uid:{uid}"),
                    })
                    .map_err(|error| {
                        // Never registered without a record of who allowed it.
                        let _ = self.butler.callers().remove(&registered.id);
                        self.butler.sync_caller(&registered.id);
                        protocol_error(
                            "audit_unavailable",
                            format!("the caller was not registered: {error}"),
                        )
                    })?;
                Ok(Reply::CallerRegistered { caller: registered })
            }
            Command::Callers {} => Ok(Reply::Callers {
                callers: self.butler.callers().list(),
            }),
            Command::RevokeCaller { id } => {
                let revoked = self
                    .butler
                    .callers()
                    .remove(&id)
                    .map_err(storage_error)?
                    .ok_or_else(|| protocol_error("not_found", format!("no caller {id}")))?;
                // From here on it holds nothing, tasks under way included,
                // and none of its standing permissions outlive it.
                self.butler.sync_caller(&revoked.id);
                let rules = self
                    .preauths
                    .revoke_subject(&Subject::Caller(revoked.id.clone()))
                    .map_err(storage_error)?;
                let by = format!("uid:{uid}");
                let mut events = vec![AuditEvent::CallerRevoked {
                    id: revoked.id.clone(),
                    by: by.clone(),
                }];
                events.extend(
                    rules
                        .into_iter()
                        .map(|rule| AuditEvent::PreauthorizationRevoked {
                            id: rule.id,
                            by: by.clone(),
                        }),
                );
                for event in events {
                    self.audit.append(event).map_err(|error| {
                        protocol_error(
                            "audit_unavailable",
                            format!("the caller was revoked, but audit failed: {error}"),
                        )
                    })?;
                }
                Ok(Reply::CallerRevoked { id: revoked.id })
            }
            Command::Capabilities {} => Ok(Reply::Capabilities {
                capabilities: match actor {
                    Some(caller) => self.butler.caller_specs(&caller.id).unwrap_or_default(),
                    None => self.view().specs.clone(),
                },
            }),
            Command::Activity { limit } => {
                if limit == 0 || limit > MAX_ACTIVITY {
                    return Err(protocol_error(
                        "invalid_argument",
                        format!("activity limit must be between 1 and {MAX_ACTIVITY}"),
                    ));
                }
                Ok(Reply::Activity {
                    items: agentos_audit::recent_activity(self.audit.path(), limit),
                })
            }
            Command::Voice {} => Ok(Reply::Voice {
                voice: self
                    .voice
                    .as_ref()
                    .map_or_else(unconfigured_voice, |voice| voice.status()),
            }),
            Command::Speech { mode, stop } => {
                let voice = self.voice.as_ref().ok_or_else(voice_unavailable)?;
                if let Some(mode) = mode {
                    voice.set_speak_mode(mode);
                }
                if stop {
                    voice.stop_speaking();
                }
                Ok(Reply::Voice {
                    voice: voice.status(),
                })
            }
            Command::Read { .. } | Command::Watch { .. } | Command::Listen { .. } => {
                unreachable!("read, watch and listen are handled at the connection boundary")
            }
        }
    }

    /// Serve a client's query of local state. This is not an agent acting: no
    /// task, no planner, no approval and no audit record, like `get`/`list`.
    /// Only capabilities that cannot have a side effect are reachable here.
    /// The talk button. Async because stopping waits for the recorder to
    /// finish writing; the words themselves arrive later as an event.
    async fn listen(&self, action: ListenAction, caller: bool) -> Result<Reply, ProtocolError> {
        if caller {
            return Err(forbidden());
        }
        let voice = self.voice.as_ref().ok_or_else(voice_unavailable)?;
        let result = match action {
            ListenAction::Start => voice.listen_start().await,
            ListenAction::Stop => voice.listen_stop(true).await,
            ListenAction::Cancel => voice.listen_stop(false).await,
        };
        result
            .map(|recording| Reply::Listening { recording })
            .map_err(|error| protocol_error(error.code, error.message))
    }

    async fn read(
        &self,
        capability: CapabilityName,
        args: Value,
        actor: Option<&Caller>,
    ) -> Result<Reply, ProtocolError> {
        // What comes back goes to the caller, so reading is something it
        // must have been given like anything else.
        if let Some(caller) = actor {
            let given = self
                .butler
                .caller_specs(&caller.id)
                .is_some_and(|specs| specs.iter().any(|spec| spec.name == capability));
            if !given {
                return Err(protocol_error(
                    "not_found",
                    format!("no capability {capability}"),
                ));
            }
        }
        if capability.as_str().is_empty() || capability.as_str().len() > MAX_CAPABILITY_NAME_BYTES {
            return Err(protocol_error(
                "invalid_argument",
                "capability name must be 1 to 128 bytes",
            ));
        }
        if !args.is_object() {
            return Err(protocol_error(
                "invalid_argument",
                "capability arguments must be a JSON object",
            ));
        }
        let target = {
            let view = self.view();
            match view.readable.get(&capability) {
                Some(target) => target.clone(),
                None if view.specs.iter().any(|spec| spec.name == capability) => {
                    return Err(protocol_error(
                        "not_readable",
                        format!("{capability} is not a read-only capability; submit an intent"),
                    ));
                }
                None => {
                    return Err(protocol_error(
                        "not_found",
                        format!("no capability {capability}"),
                    ));
                }
            }
        };
        match timeout(READ_TIMEOUT, target.invoke(args)).await {
            Ok(Ok(value)) => Ok(Reply::Value { capability, value }),
            Ok(Err(CapabilityError::InvalidArgs(message))) => {
                Err(protocol_error("invalid_argument", message))
            }
            Ok(Err(error)) => Err(protocol_error("capability_failed", error.to_string())),
            Err(_) => Err(protocol_error(
                "capability_failed",
                format!("{capability} did not answer within 10 seconds"),
            )),
        }
    }
}

async fn send_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    response: Response,
) -> io::Result<()> {
    timeout(WRITE_TIMEOUT, async {
        // The shared serializer is bounded and writes no bytes on overflow.
        // Preserve that bound instead of pre-serializing a large task here.
        match write_frame(writer, &response).await {
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                write_frame(
                    writer,
                    &Response::error(
                        response.id,
                        "response_too_large",
                        "response exceeds the 1 MiB protocol limit; use the paged task list or a narrower read",
                    ),
                )
                .await
            }
            result => result,
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "client is not reading responses"))?
}

async fn serve_connection(stream: UnixStream, controller: Arc<Controller>) -> io::Result<()> {
    let uid = stream.peer_cred()?.uid();
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    loop {
        let frame = timeout(REQUEST_TIMEOUT, read_frame(&mut reader))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "idle control connection"))??;
        let Some(frame) = frame else { return Ok(()) };
        let request: Request = match serde_json::from_slice(&frame) {
            Ok(request) => request,
            Err(error) => {
                let id = serde_json::from_slice::<serde_json::Value>(&frame)
                    .ok()
                    .and_then(|value| value.get("id").and_then(|id| id.as_u64()))
                    .unwrap_or(0);
                send_response(
                    &mut writer,
                    Response::error(id, "invalid_request", error.to_string()),
                )
                .await?;
                continue;
            }
        };
        if request.version != VERSION {
            send_response(
                &mut writer,
                Response::error(
                    request.id,
                    "unsupported_version",
                    format!("supported protocol version is {VERSION}"),
                ),
            )
            .await?;
            continue;
        }
        let actor = match controller.actor(request.caller.as_deref()) {
            Ok(actor) => actor,
            Err(error) => {
                send_response(
                    &mut writer,
                    Response {
                        version: VERSION,
                        id: request.id,
                        body: agentos_ipc::ResponseBody::Error { error },
                    },
                )
                .await?;
                continue;
            }
        };
        // Change hints name every task; a caller reads its own instead.
        if actor.is_some() && matches!(request.command, Command::Watch { .. }) {
            send_response(
                &mut writer,
                Response {
                    version: VERSION,
                    id: request.id,
                    body: agentos_ipc::ResponseBody::Error { error: forbidden() },
                },
            )
            .await?;
            continue;
        }
        if let Command::Watch { after, voice } = request.command {
            let mut changes = controller.tasks.subscribe();
            // Subscribed with the task changes, so nothing falls in between.
            let mut voice_events = controller
                .voice
                .as_ref()
                .filter(|_| voice)
                .map(|voice| voice.subscribe());
            let revision = controller.tasks.revision();
            let mut last_sent = revision;
            send_response(
                &mut writer,
                Response::ok(request.id, Reply::Watching { revision }),
            )
            .await?;
            if after != Some(revision) {
                send_event(&mut writer, Event { version: VERSION, revision, kind: EventKind::ResyncRequired { reason: "refresh authoritative task pages; events from a previous connection are not replayed".into() } }).await?;
            }
            let mut byte = [0_u8; 1];
            loop {
                tokio::select! {
                    read = reader.read(&mut byte) => {
                        if read? > 0 { return Err(io::Error::new(io::ErrorKind::InvalidInput, "watch connections accept no further requests")); }
                        return Ok(());
                    }
                    spoken = next_voice_event(&mut voice_events) => {
                        let Some(kind) = spoken else { continue };
                        send_event(&mut writer, Event { version: VERSION, revision: last_sent, kind }).await?;
                    }
                    change = changes.recv() => {
                        let event = match change {
                            Ok(change) if change.revision <= last_sent => continue,
                            Ok(change) => Event { version: VERSION, revision: change.revision, kind: EventKind::TaskChanged { id: change.id } },
                            Err(broadcast::error::RecvError::Lagged(_)) => Event { version: VERSION, revision: controller.tasks.revision(), kind: EventKind::ResyncRequired { reason: "watcher fell behind; refresh authoritative task pages".into() } },
                            Err(broadcast::error::RecvError::Closed) => return Ok(()),
                        };
                        last_sent = event.revision;
                        send_event(&mut writer, event).await?;
                    }
                }
            }
        }
        let result = match request.command {
            Command::Read { capability, args } => {
                controller.read(capability, args, actor.as_ref()).await
            }
            Command::Listen { action } => controller.listen(action, actor.is_some()).await,
            command => controller.handle(command, uid, actor.as_ref()),
        };
        let response = match result {
            Ok(reply) => Response::ok(request.id, reply),
            Err(error) => Response {
                version: VERSION,
                id: request.id,
                body: agentos_ipc::ResponseBody::Error { error },
            },
        };
        send_response(&mut writer, response).await?;
    }
}

/// The next voice event for a watch that asked for them; never, for one that
/// did not. A watcher that fell behind skips what it missed: levels are
/// lossy, and the state event that follows tells it where things stand.
async fn next_voice_event(
    events: &mut Option<broadcast::Receiver<EventKind>>,
) -> Option<EventKind> {
    let Some(receiver) = events else {
        return std::future::pending().await;
    };
    match receiver.recv().await {
        Ok(kind) => Some(kind),
        Err(broadcast::error::RecvError::Lagged(_)) => None,
        Err(broadcast::error::RecvError::Closed) => {
            *events = None;
            None
        }
    }
}

async fn send_event<W: AsyncWrite + Unpin>(writer: &mut W, event: Event) -> io::Result<()> {
    timeout(WRITE_TIMEOUT, write_frame(writer, &event))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "watcher is not reading events"))?
}
