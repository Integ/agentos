//! A bounded, local control service. Connections never own task lifetimes.

use std::fs::{self, DirBuilder};
use std::future::Future;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentos_audit::{AuditEvent, AuditLog};
use agentos_core::{
    now_ms, AgentId, CapabilityError, CapabilityName, DeviceId, Intent, IntentId, Urgency,
};
use agentos_ipc::{
    current_uid, read_frame, write_frame, Command, Event, EventKind, ProtocolError, Reply, Request,
    Response, TaskSummary, MAX_CAPABILITY_NAME_BYTES, MAX_PAGE_SIZE, MAX_TEXT_BYTES, VERSION,
};
use agentos_policy::PolicyEngine;
use agentos_runtime::{Task, TaskId, TaskState, TaskStore};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWrite, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, Semaphore};
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::butler::{Butler, CapabilityView};
use crate::control::approval_matches_with_token;
use agentos_planner::Planner;
use agentos_services::expression::ExpressionHub;
use agentos_services::identity::{PinCheck, PinVault};
use agentos_services::registry::CapabilityRegistry;

const MAX_CONNECTIONS: usize = 64;
const QUEUE_CAPACITY: usize = 64;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Service {
    controller: Arc<Controller>,
    work_rx: mpsc::Receiver<Work>,
    butler: Butler,
}

pub struct Server {
    listener: UnixListener,
    socket: SocketGuard,
    service: Service,
}

struct Controller {
    tasks: Arc<TaskStore>,
    audit: Arc<AuditLog>,
    /// What the butler can currently do; it keeps this current as plugins
    /// are installed, disabled or rolled back.
    view: Arc<Mutex<CapabilityView>>,
    /// Proves identity for approvals of irreversible steps.
    pins: Option<Arc<PinVault>>,
    work_tx: mpsc::Sender<Work>,
}

enum Work {
    Run(TaskId),
    Approve {
        id: TaskId,
        step: usize,
        responder: Intent,
        token: String,
    },
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
        let (work_tx, work_rx) = mpsc::channel(QUEUE_CAPACITY);
        Self {
            controller: Arc::new(Controller {
                tasks: butler.tasks().clone(),
                audit,
                view: butler.view(),
                pins,
                work_tx,
            }),
            work_rx,
            butler,
        }
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
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "socket parent must be owned by this user and not writable by group/others; use a private directory"));
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
            mut work_rx,
            mut butler,
        } = service;
        let mut worker = tokio::spawn(async move {
            while let Some(work) = work_rx.recv().await {
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
                }
            }
        });
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
                    // Current capabilities are asynchronous. Bound this final
                    // cancellation wait too, so a future blocking plugin cannot
                    // keep the control service alive indefinitely.
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

impl Controller {
    fn view(&self) -> std::sync::MutexGuard<'_, CapabilityView> {
        self.view.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn task(&self, id: TaskId) -> Result<Task, ProtocolError> {
        self.tasks
            .get(id)
            .ok_or_else(|| protocol_error("not_found", format!("no task {id}")))
    }

    fn handle(&self, command: Command, uid: u32) -> Result<Reply, ProtocolError> {
        match command {
            Command::Ping {} => Ok(Reply::Pong {
                server_version: env!("CARGO_PKG_VERSION").into(),
                revision: self.tasks.revision(),
            }),
            Command::Submit { text } => {
                if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
                    return Err(protocol_error(
                        "invalid_argument",
                        "intent must be non-empty and at most 64 KiB",
                    ));
                }
                let permit = self.work_tx.try_reserve().map_err(|_| {
                    protocol_error(
                        "queue_unavailable",
                        "task queue is full or unavailable; no task was created",
                    )
                })?;
                let task = self
                    .tasks
                    .create(responder(uid, text.trim().into()))
                    .map_err(storage_error)?;
                permit.send(Work::Run(task.id));
                Ok(Reply::Accepted { task })
            }
            Command::List {
                before,
                limit,
                state,
            } => {
                if !(1..=MAX_PAGE_SIZE).contains(&limit) {
                    return Err(protocol_error(
                        "invalid_argument",
                        "page limit must be between 1 and 50",
                    ));
                }
                let (tasks, next_before, revision) = self.tasks.page(before, limit, state);
                Ok(Reply::Tasks {
                    tasks: tasks.iter().map(TaskSummary::from).collect(),
                    next_before,
                    revision,
                })
            }
            Command::Get { id } => Ok(Reply::Task {
                task: self.task(id)?,
            }),
            Command::Cancel { id } => {
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
                Ok(Reply::Task { task })
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
                let permit = self.work_tx.try_reserve().map_err(|_| {
                    protocol_error(
                        "queue_unavailable",
                        "approval queue is full or unavailable; retry explicitly",
                    )
                })?;
                permit.send(Work::Approve {
                    id,
                    step: index,
                    responder,
                    token: confirmation,
                });
                Ok(Reply::ApprovalQueued { id, step })
            }
            Command::Capabilities {} => Ok(Reply::Capabilities {
                capabilities: self.view().specs.clone(),
            }),
            Command::Read { .. } | Command::Watch { .. } => {
                unreachable!("read and watch are handled at the connection boundary")
            }
        }
    }

    /// Serve a client's query of local state. This is not an agent acting: no
    /// task, no planner, no approval and no audit record, like `get`/`list`.
    /// Only capabilities that cannot have a side effect are reachable here.
    async fn read(&self, capability: CapabilityName, args: Value) -> Result<Reply, ProtocolError> {
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
        if let Command::Watch { after } = request.command {
            let mut changes = controller.tasks.subscribe();
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
            Command::Read { capability, args } => controller.read(capability, args).await,
            command => controller.handle(command, uid),
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

async fn send_event<W: AsyncWrite + Unpin>(writer: &mut W, event: Event) -> io::Result<()> {
    timeout(WRITE_TIMEOUT, write_frame(writer, &event))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "watcher is not reading events"))?
}
