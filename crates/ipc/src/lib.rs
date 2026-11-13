//! Versioned, bounded JSON Lines protocol shared by the daemon and its clients.
//!
//! A connection carries requests/responses, or switches to live invalidation
//! events after `Watch`. Live events are hints to refresh authoritative task
//! pages; reconnecting clients must handle `ResyncRequired` instead of guessing
//! that a missed event meant nothing changed. Mutations must never be blindly
//! replayed when a connection fails before its response arrives.

use std::io;
use std::path::{Path, PathBuf};

use agentos_core::{CapabilityName, CapabilitySpec, RiskLevel};
pub use agentos_policy::{ArgRule, Matcher, Preauthorization, Subject};
use agentos_runtime::{StepState, Task, TaskId, TaskState};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1_048_576;
pub const MAX_TEXT_BYTES: usize = 65_536;
pub const MAX_PAGE_SIZE: usize = 50;
pub const MAX_CAPABILITY_NAME_BYTES: usize = 128;

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub id: u64,
    pub command: Command,
    /// Make this request as a registered outside caller instead of as the
    /// user. It only ever narrows what the request may do, so it needs no
    /// secret: whoever can reach the socket could have asked for more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
}

/// Where a caller's tasks say they come from: `caller:<id>`.
pub const CALLER_SOURCE_PREFIX: &str = "caller:";

/// An outside program the user lets submit intents, and what it may use.
/// A caller holds nothing it was not given here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Caller {
    pub id: String,
    pub name: String,
    /// Capability names, or prefixes ending in `*` (`notes.*`).
    pub capabilities: Vec<String>,
    /// The highest risk its tasks may touch at all.
    pub max_risk: RiskLevel,
    #[serde(default)]
    pub created_at_ms: u64,
}

impl Caller {
    /// Whether the caller was given `capability`.
    pub fn wants(&self, capability: &str) -> bool {
        self.capabilities
            .iter()
            .any(|pattern| match pattern.strip_suffix('*') {
                Some(prefix) => capability.starts_with(prefix),
                None => capability == pattern,
            })
    }

    pub fn source(&self) -> String {
        format!("{CALLER_SOURCE_PREFIX}{}", self.id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    // Empty struct variants make Serde enforce deny_unknown_fields; unit
    // variants silently discard extra fields in an internally tagged object.
    Ping {},
    Submit {
        text: String,
        /// The thread this intent continues, named by its first task. Absent
        /// starts a new thread.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<TaskId>,
    },
    List {
        #[serde(default)]
        before: Option<TaskId>,
        limit: usize,
        #[serde(default)]
        state: Option<TaskState>,
        /// Only tasks of this thread.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<TaskId>,
    },
    Get {
        id: TaskId,
    },
    Cancel {
        id: TaskId,
    },
    Approve {
        id: TaskId,
        /// Human-facing, one-based step number.
        step: usize,
        /// Exact token received with this task's concrete confirmation prompt.
        confirmation: String,
        /// Proof of identity for an irreversible step: the PIN. Checked by
        /// the service and never stored, logged or echoed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pin: Option<String>,
    },
    /// Answer the question a task in `awaiting_input` is waiting on.
    Reply {
        id: TaskId,
        /// Human-facing, one-based number of the step that asked.
        step: usize,
        text: String,
    },
    /// Give standing permission for calls of one capability whose arguments
    /// satisfy every rule, so they stop asking. Only the user's own client
    /// can do this; no agent and no capability can.
    Preauthorize {
        capability: CapabilityName,
        /// The registered caller whose tasks this applies to. Absent means
        /// the user's own tasks. A rule never applies to both.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
        #[serde(default)]
        args: Vec<ArgRule>,
        /// Accept arguments that no rule mentions. Off unless stated.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        other_args: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_in_seconds: Option<u64>,
        /// Required when a PIN is set. Checked and never stored or logged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pin: Option<String>,
    },
    /// The pre-authorizations still in force.
    Preauthorizations {},
    RevokePreauthorization {
        id: String,
    },
    /// Let an outside program submit intents with exactly these
    /// capabilities, or replace what a registered one may use.
    RegisterCaller {
        id: String,
        name: String,
        #[serde(default)]
        capabilities: Vec<String>,
        max_risk: RiskLevel,
        /// Required when a PIN is set. Checked and never stored or logged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pin: Option<String>,
    },
    Callers {},
    /// Withdraw a caller. Its tasks under way lose everything at once, and
    /// its pre-authorizations go with it.
    RevokeCaller {
        id: String,
    },
    Capabilities {},
    /// Query a granted read-only (L0) capability directly. A client reading
    /// state is not an intent: no task is created, nothing is planned and no
    /// side effect is possible. Anything else must be submitted as an intent.
    Read {
        capability: CapabilityName,
        #[serde(default = "empty_object")]
        args: Value,
    },
    Watch {
        #[serde(default)]
        after: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub version: u32,
    pub id: u64,
    pub body: ResponseBody,
}

impl Response {
    pub fn ok(id: u64, data: Reply) -> Self {
        Self {
            version: VERSION,
            id,
            body: ResponseBody::Ok { data },
        }
    }

    pub fn error(id: u64, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            version: VERSION,
            id,
            body: ResponseBody::Error {
                error: ProtocolError {
                    code: code.into(),
                    message: message.into(),
                },
            },
        }
    }
}

// A response is built, written and dropped. Boxing the common case to shrink
// the rare error would cost an allocation per reply and change nothing else.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResponseBody {
    Ok { data: Reply },
    Error { error: ProtocolError },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Pong {
        server_version: String,
        revision: u64,
    },
    Accepted {
        task: Task,
    },
    Tasks {
        tasks: Vec<TaskSummary>,
        next_before: Option<TaskId>,
        revision: u64,
    },
    Task {
        task: Task,
    },
    Capabilities {
        capabilities: Vec<CapabilitySpec>,
    },
    /// What a read-only capability returned to a direct `Read`.
    Value {
        capability: CapabilityName,
        value: Value,
    },
    ApprovalQueued {
        id: TaskId,
        step: usize,
    },
    /// The answer was accepted for the task's current question. It does not
    /// say what the task did with it.
    ReplyQueued {
        id: TaskId,
        step: usize,
    },
    Preauthorized {
        preauthorization: Preauthorization,
    },
    Preauthorizations {
        preauthorizations: Vec<Preauthorization>,
    },
    PreauthorizationRevoked {
        id: String,
    },
    CallerRegistered {
        caller: Caller,
    },
    Callers {
        callers: Vec<Caller>,
    },
    CallerRevoked {
        id: String,
    },
    Watching {
        revision: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSummary {
    pub id: TaskId,
    /// The thread the task belongs to. Absent from older services, where
    /// every task was its own thread.
    #[serde(default)]
    pub thread: TaskId,
    pub text: String,
    pub state: TaskState,
    pub completed_steps: usize,
    pub total_steps: usize,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl From<&Task> for TaskSummary {
    fn from(task: &Task) -> Self {
        Self {
            id: task.id,
            thread: task.thread,
            text: task.intent.text.chars().take(120).collect(),
            state: task.state,
            completed_steps: task
                .steps
                .iter()
                .filter(|step| step.state == StepState::Succeeded)
                .count(),
            total_steps: task.steps.len(),
            created_at_ms: task.created_at_ms,
            updated_at_ms: task.updated_at_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub version: u32,
    pub revision: u64,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventKind {
    TaskChanged { id: TaskId },
    ResyncRequired { reason: String },
}

/// Read one newline-terminated frame without allowing an unbounded allocation.
/// A clean EOF between frames returns `None`; a partial final frame is rejected.
/// The frame limit excludes the newline delimiter. Empty frames are returned and
/// rejected by JSON parsing at the protocol boundary.
///
/// This operation is **not cancellation safe**: it may have consumed a partial
/// frame before awaiting more bytes. If cancelled, discard the connection. A
/// long-lived event stream should use a dedicated reader task rather than race
/// this future against UI input and reuse the reader after the other branch wins.
pub async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unterminated protocol frame",
                ))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.unwrap_or(available.len());
        if frame.len().saturating_add(consumed) > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "protocol frame exceeds 1 MiB",
            ));
        }
        frame.extend_from_slice(&available[..consumed]);
        reader.consume(consumed + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(Some(frame));
        }
    }
}

/// Serialize and size-check the complete frame before sending any bytes.
pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> io::Result<()> {
    let mut buffer = LimitedBuffer(Vec::new());
    serde_json::to_writer(&mut buffer, value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    writer.write_all(&buffer.0).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

/// Limit serialization itself, rather than allocating a potentially enormous
/// response and discovering its size only after that allocation has happened.
struct LimitedBuffer(Vec<u8>);

impl io::Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "protocol response exceeds 1 MiB",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn default_state_dir() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("AGENTOS_STATE_DIR").filter(|value| !value.is_empty()) {
        return Ok(path.into());
    }
    if let Some(path) = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Ok(path.join("agentos"));
    }
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".local/state/agentos"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "set AGENTOS_STATE_DIR or pass --state-dir",
            )
        })
}

pub fn default_socket_path(state: &Path) -> PathBuf {
    std::env::var_os("AGENTOS_TASK_SOCKET")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state.join("control.sock"))
}

#[cfg(unix)]
pub fn current_uid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no inputs and returns the process's effective UID.
    unsafe { geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::{now_ms, DeviceId, Intent, IntentId, Urgency};
    use agentos_runtime::TaskStore;
    use serde_json::json;
    use tokio::io::BufReader;

    #[test]
    fn protocol_envelopes_round_trip_and_reject_unknown_fields() {
        let request = Request {
            version: VERSION,
            id: 7,
            command: Command::Approve {
                id: TaskId(3),
                step: 2,
                confirmation: "nonce".into(),
                pin: None,
            },
            caller: None,
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["command"]["method"], "approve");
        let decoded: Request = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(decoded.id, 7);
        let mut unknown = value;
        unknown["command"]["speaker"] = json!("someone else");
        assert!(serde_json::from_value::<Request>(unknown).is_err());
        let response = Response::error(7, "permission_denied", "not allowed");
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["body"]["status"], "error");
        assert_eq!(serde_json::from_value::<Response>(value).unwrap().id, 7);
        let event = Event {
            version: VERSION,
            revision: 8,
            kind: EventKind::TaskChanged { id: TaskId(3) },
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["kind"]["event"], "task_changed");
        assert_eq!(serde_json::from_value::<Event>(value).unwrap().revision, 8);
    }

    #[tokio::test]
    async fn framing_preserves_boundaries_and_rejects_truncated_or_oversized_input() {
        let mut reader = BufReader::with_capacity(3, &b"{\"a\":1}\n{}\n"[..]);
        assert_eq!(
            read_frame(&mut reader).await.unwrap().unwrap(),
            b"{\"a\":1}"
        );
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), b"{}");
        assert!(read_frame(&mut reader).await.unwrap().is_none());
        let mut reader = BufReader::new(&b"{}"[..]);
        assert_eq!(
            read_frame(&mut reader).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let mut huge = vec![b'x'; MAX_FRAME_BYTES + 1];
        huge.push(b'\n');
        let mut reader = BufReader::new(huge.as_slice());
        assert_eq!(
            read_frame(&mut reader).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut exact = vec![b'x'; MAX_FRAME_BYTES];
        exact.push(b'\n');
        let mut reader = BufReader::new(exact.as_slice());
        assert_eq!(
            read_frame(&mut reader).await.unwrap().unwrap().len(),
            MAX_FRAME_BYTES
        );
    }

    #[test]
    fn every_command_rejects_unknown_fields() {
        for command in [
            json!({"method":"ping"}),
            json!({"method":"capabilities"}),
            json!({"method":"submit","text":"现在几点"}),
            json!({"method":"list","limit":1}),
            json!({"method":"get","id":1}),
            json!({"method":"cancel","id":1}),
            json!({"method":"approve","id":1,"step":1,"confirmation":"nonce"}),
            json!({"method":"read","capability":"notes.list"}),
            json!({"method":"read","capability":"notes.list","args":{}}),
            json!({"method":"watch"}),
        ] {
            let request = json!({"version":VERSION,"id":7,"command":command});
            assert!(serde_json::from_value::<Request>(request.clone()).is_ok());
            for value in [json!("admin"), json!(null), json!({"role":"admin"})] {
                let mut unknown = request.clone();
                unknown["command"]["speaker"] = value;
                let wire = serde_json::to_vec(&unknown).unwrap();
                assert!(
                    serde_json::from_slice::<Request>(&wire).is_err(),
                    "accepted unknown field on the wire: {unknown}"
                );
                assert!(
                    serde_json::from_value::<Request>(unknown.clone()).is_err(),
                    "accepted unknown field in JSON value: {unknown}"
                );
            }
        }
    }

    #[test]
    fn empty_commands_preserve_the_protocol_v1_wire_shape() {
        for (command, method) in [
            (Command::Ping {}, "ping"),
            (Command::Capabilities {}, "capabilities"),
        ] {
            let expected = json!({"method":method});
            assert_eq!(serde_json::to_value(command).unwrap(), expected);
            let decoded: Command = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        }
    }

    #[test]
    fn direct_reads_default_to_empty_arguments_and_echo_the_capability() {
        let command: Command =
            serde_json::from_value(json!({"method":"read","capability":"notes.list"})).unwrap();
        let Command::Read { capability, args } = &command else {
            panic!("not a read")
        };
        assert_eq!((capability.as_str(), args), ("notes.list", &json!({})));
        assert_eq!(
            serde_json::to_value(&command).unwrap(),
            json!({"method":"read","capability":"notes.list","args":{}})
        );
        // A read names exactly one capability; it cannot smuggle an intent.
        for invalid in [
            json!({"method":"read"}),
            json!({"method":"read","capability":7}),
            json!({"method":"read","capability":"notes.list","text":"清空笔记"}),
        ] {
            assert!(serde_json::from_value::<Command>(invalid).is_err());
        }
        let reply = Reply::Value {
            capability: CapabilityName::new("notes.list"),
            value: json!({"notes":["牛奶"]}),
        };
        let wire = serde_json::to_value(Response::ok(3, reply)).unwrap();
        assert_eq!(
            wire["body"]["data"],
            json!({"type":"value","capability":"notes.list","value":{"notes":["牛奶"]}})
        );
        assert!(serde_json::from_value::<Response>(wire).is_ok());
    }

    #[test]
    fn unhandled_is_a_wire_state_that_filters_and_summaries_understand() {
        let command: Command =
            serde_json::from_value(json!({"method":"list","limit":5,"state":"unhandled"})).unwrap();
        assert!(matches!(
            command,
            Command::List {
                state: Some(TaskState::Unhandled),
                ..
            }
        ));
        let store = TaskStore::memory();
        let task = store
            .create(Intent {
                id: IntentId(0),
                source: DeviceId::new("test"),
                speaker: None,
                text: "讲个笑话".into(),
                urgency: Urgency::Normal,
                at_ms: now_ms(),
            })
            .unwrap();
        let task = store
            .update(task.id, |task| task.state = TaskState::Unhandled)
            .unwrap();
        let summary = serde_json::to_value(TaskSummary::from(&task)).unwrap();
        assert_eq!(summary["state"], "unhandled");
        assert_eq!(
            (
                summary["completed_steps"].as_u64(),
                summary["total_steps"].as_u64()
            ),
            (Some(0), Some(0))
        );
    }

    #[tokio::test]
    async fn oversized_response_writes_no_partial_data() {
        let mut writer = Vec::new();
        assert!(write_frame(&mut writer, &"x".repeat(MAX_FRAME_BYTES))
            .await
            .is_err());
        assert!(writer.is_empty());
        write_frame(&mut writer, &json!({"ok":true})).await.unwrap();
        assert_eq!(writer, b"{\"ok\":true}\n");
    }

    #[test]
    fn summaries_truncate_unicode_without_exposing_large_step_results() {
        let store = TaskStore::memory();
        let task = store
            .create(Intent {
                id: IntentId(0),
                source: DeviceId::new("test"),
                speaker: None,
                text: "意".repeat(150),
                urgency: Urgency::Normal,
                at_ms: now_ms(),
            })
            .unwrap();
        let summary = TaskSummary::from(&task);
        assert_eq!(summary.text.chars().count(), 120);
        assert_eq!(summary.id, task.id);
        assert_eq!(summary.total_steps, 0);
    }
}
