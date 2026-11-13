//! AgentOS as an MCP server, for one registered outside caller.
//!
//! `agentos mcp --caller <id>` speaks MCP on standard input and output and
//! is nothing but another client of the task service: every tool call
//! becomes a protocol request made as that caller. What the caller may use,
//! see and decide is enforced by the service, not here; this file only
//! translates. In particular it has no tool for approving anything, and a
//! mutation whose response was lost is reported as unknown and never sent
//! again.

use std::io;
use std::time::Duration;

use agentos_core::CapabilityName;
use agentos_ipc::{Command, Reply, MAX_PAGE_SIZE};
use agentos_runtime::{Task, TaskId, TaskState};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::client::{Client, ClientError};
use crate::wait::wait_for_task;

/// Protocol revisions this server can speak, newest first.
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
/// How long `submit_intent` may wait for an outcome before returning the
/// task as it is.
const MAX_WAIT_SECONDS: u64 = 120;
const DEFAULT_WAIT_SECONDS: u64 = 20;
/// A step result is context for another model: enough to use, not unbounded.
const MAX_RESULT_CHARS: usize = 2_000;
const MAX_STEPS: usize = 50;

const INSTRUCTIONS: &str = "AgentOS 把用户电脑上的事交给它的管家 Agent 去做。\
用 submit_intent 提交一件要做的事（自然语言），它返回一个任务；任务在后台执行，用 get_task 查看结果。\
你只能使用用户为你这个调用方开放的能力，list_capabilities 会列出它们。\
任务状态为 awaiting_confirmation 时，需要用户本人在 AgentOS 里批准，你无法代为批准，只能等待或取消。\
状态为 awaiting_input 时，管家在问你一个问题，用 reply_to_task 回答。\
提交、取消和回答如果返回“结果未知”，先用 get_task 或 list_tasks 查询，不要重复提交。";

fn tools() -> Value {
    let id = json!({"type": "integer", "minimum": 1, "description": "任务编号"});
    json!([
        {
            "name": "submit_intent",
            "description": "提交一件要 AgentOS 去做的事。返回任务；默认最多等 20 秒看它是否结束。不要因为没等到结果而重复提交。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "要做的事，用自然语言写清楚"},
                    "thread": {"type": "integer", "minimum": 1, "description": "接着哪段对话说：那段对话第一个任务的编号。省略则开始新的对话"},
                    "wait_seconds": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_SECONDS, "description": "最多等多少秒；0 表示提交后立刻返回"}
                },
                "required": ["text"],
                "additionalProperties": false
            }
        },
        {
            "name": "get_task",
            "description": "查看你提交的一个任务：状态、步骤、结果，以及它是否在等用户批准或等你回答。",
            "inputSchema": {"type": "object", "properties": {"id": id}, "required": ["id"], "additionalProperties": false}
        },
        {
            "name": "list_tasks",
            "description": "列出你提交过的任务，新的在前。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_PAGE_SIZE},
                    "thread": {"type": "integer", "minimum": 1, "description": "只列出这段对话的任务"}
                },
                "additionalProperties": false
            }
        },
        {
            "name": "cancel_task",
            "description": "取消你提交的一个任务。已经开始的步骤可能仍然生效。",
            "inputSchema": {"type": "object", "properties": {"id": id}, "required": ["id"], "additionalProperties": false}
        },
        {
            "name": "reply_to_task",
            "description": "回答一个处于 awaiting_input 的任务提出的问题。step 用任务里 question.step 给出的编号。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": id,
                    "step": {"type": "integer", "minimum": 1},
                    "text": {"type": "string"}
                },
                "required": ["id", "step", "text"],
                "additionalProperties": false
            }
        },
        {
            "name": "list_capabilities",
            "description": "列出用户为你开放的能力和它们的风险等级。",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        },
        {
            "name": "read_capability",
            "description": "直接读取一个为你开放的只读能力，不创建任务。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "capability": {"type": "string"},
                    "args": {"type": "object"}
                },
                "required": ["capability"],
                "additionalProperties": false
            }
        }
    ])
}

fn clip(text: &str) -> String {
    let mut chars = text.chars();
    let mut clipped: String = chars.by_ref().take(MAX_RESULT_CHARS).collect();
    if chars.next().is_some() {
        clipped.push('…');
    }
    clipped
}

/// A task as the caller is told about it: what happened and, if it is
/// waiting, on whom. Never the approval token.
fn task_view(task: &Task) -> Value {
    let waiting = match task.state {
        TaskState::AwaitingConfirmation => {
            Some("需要用户本人在 AgentOS 里批准这一步；你不能代为批准，可以稍后用 get_task 再看，或取消。")
        }
        TaskState::AwaitingInput => Some("管家在等你回答 question；用 reply_to_task 回答。"),
        TaskState::Queued | TaskState::Planning | TaskState::Running => {
            Some("任务还在进行；稍后用 get_task 查看，不要重新提交。")
        }
        _ => None,
    };
    let mut view = json!({
        "id": task.id,
        "thread": task.thread,
        "state": task.state,
        "intent": clip(&task.intent.text),
        "message": task.message.as_deref().map(clip),
        "steps": task.steps.iter().take(MAX_STEPS).map(|step| json!({
            "capability": step.capability,
            "state": step.state,
            "result": step.result.as_ref().map(|result| clip(&result.to_string())),
        })).collect::<Vec<_>>(),
    });
    if let Some(question) = &task.question {
        view["question"] = json!({"step": question.step + 1, "text": clip(&question.text)});
    }
    if let Some(note) = waiting {
        view["note"] = json!(note);
    }
    view
}

fn text_result(value: &Value, is_error: bool) -> Value {
    let text = match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn failure(error: &ClientError) -> Value {
    let mut text = error.to_string();
    if error.ambiguous() {
        // The request may have taken effect. Asking again is the caller's
        // decision to make with the facts, never something done for it.
        text.push_str(
            "\n结果未知：这个操作可能已经生效。先用 get_task 或 list_tasks 查询，不要直接重试。",
        );
    }
    text_result(&json!(text), true)
}

struct Arguments<'a>(&'a Map<String, Value>);

impl Arguments<'_> {
    fn known(&self, names: &[&str]) -> Result<(), String> {
        match self.0.keys().find(|key| !names.contains(&key.as_str())) {
            Some(unknown) => Err(format!("未知参数：{unknown}")),
            None => Ok(()),
        }
    }

    fn text(&self, name: &str) -> Result<String, String> {
        self.0
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("需要字符串参数 {name}"))
    }

    fn number(&self, name: &str) -> Result<Option<u64>, String> {
        match self.0.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .map(Some)
                .ok_or_else(|| format!("参数 {name} 应为非负整数")),
        }
    }

    fn task(&self, name: &str) -> Result<TaskId, String> {
        self.number(name)?
            .filter(|id| *id > 0)
            .map(TaskId)
            .ok_or_else(|| format!("需要任务编号 {name}"))
    }
}

async fn call_tool(client: &Client, name: &str, arguments: &Map<String, Value>) -> Value {
    let args = Arguments(arguments);
    let outcome: Result<Result<Value, ClientError>, String> = async {
        Ok(match name {
            "submit_intent" => {
                args.known(&["text", "thread", "wait_seconds"])?;
                let text = args.text("text")?;
                let thread = args.number("thread")?.map(TaskId);
                let wait = args
                    .number("wait_seconds")?
                    .unwrap_or(DEFAULT_WAIT_SECONDS)
                    .min(MAX_WAIT_SECONDS);
                match client.request(Command::Submit { text, thread }).await {
                    Ok(Reply::Accepted { task }) if wait == 0 => Ok(task_view(&task)),
                    // Waiting only reads. If it fails, the task is still
                    // there: say which one instead of losing its number.
                    Ok(Reply::Accepted { task }) => {
                        match wait_for_task(client, task.id, Duration::from_secs(wait)).await {
                            Ok(outcome) => Ok(task_view(outcome.task())),
                            Err(_) => Ok(task_view(&task)),
                        }
                    }
                    Ok(_) => unreachable!("the client verifies reply types"),
                    Err(error) => Err(error),
                }
            }
            "get_task" => {
                args.known(&["id"])?;
                let id = args.task("id")?;
                client.request(Command::Get { id }).await.map(|reply| {
                    let Reply::Task { task } = reply else {
                        unreachable!("the client verifies reply types")
                    };
                    task_view(&task)
                })
            }
            "list_tasks" => {
                args.known(&["limit", "thread"])?;
                let limit = args.number("limit")?.unwrap_or(10) as usize;
                if !(1..=MAX_PAGE_SIZE).contains(&limit) {
                    return Err(format!("limit 应为 1–{MAX_PAGE_SIZE}"));
                }
                let command = Command::List {
                    before: None,
                    limit,
                    state: None,
                    thread: args.number("thread")?.map(TaskId),
                };
                client.request(command).await.map(|reply| {
                    let Reply::Tasks { tasks, .. } = reply else {
                        unreachable!("the client verifies reply types")
                    };
                    json!(tasks
                        .iter()
                        .map(|task| json!({
                            "id": task.id, "thread": task.thread, "state": task.state,
                            "intent": task.text,
                        }))
                        .collect::<Vec<_>>())
                })
            }
            "cancel_task" => {
                args.known(&["id"])?;
                let id = args.task("id")?;
                client.request(Command::Cancel { id }).await.map(|reply| {
                    let Reply::Task { task } = reply else {
                        unreachable!("the client verifies reply types")
                    };
                    task_view(&task)
                })
            }
            "reply_to_task" => {
                args.known(&["id", "step", "text"])?;
                let command = Command::Reply {
                    id: args.task("id")?,
                    step: args
                        .number("step")?
                        .filter(|step| *step > 0)
                        .ok_or("需要从 1 开始的 step")? as usize,
                    text: args.text("text")?,
                };
                client
                    .request(command)
                    .await
                    .map(|_| json!("回答已交给任务；用 get_task 查看它接下来的进展。"))
            }
            "list_capabilities" => {
                args.known(&[])?;
                client.request(Command::Capabilities {}).await.map(|reply| {
                    let Reply::Capabilities { capabilities } = reply else {
                        unreachable!("the client verifies reply types")
                    };
                    json!(capabilities
                        .iter()
                        .map(|spec| json!({
                            "name": spec.name, "risk": spec.risk,
                            "description": spec.description,
                        }))
                        .collect::<Vec<_>>())
                })
            }
            "read_capability" => {
                args.known(&["capability", "args"])?;
                let command = Command::Read {
                    capability: CapabilityName::new(args.text("capability")?),
                    args: match arguments.get("args") {
                        None | Some(Value::Null) => json!({}),
                        Some(value) if value.is_object() => value.clone(),
                        Some(_) => return Err("参数 args 应为 JSON 对象".into()),
                    },
                };
                client.request(command).await.map(|reply| {
                    let Reply::Value { value, .. } = reply else {
                        unreachable!("the client verifies reply types")
                    };
                    json!(clip(&value.to_string()))
                })
            }
            _ => return Err(format!("没有这个工具：{name}")),
        })
    }
    .await;
    match outcome {
        Ok(Ok(value)) => text_result(&value, false),
        Ok(Err(error)) => failure(&error),
        Err(usage) => text_result(&json!(usage), true),
    }
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Answer one JSON-RPC message. `None` for a notification, which gets no
/// answer whatever it says.
pub async fn respond(client: &Client, message: &Value) -> Option<Value> {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        // A response to something this server never asked, or not a request.
        return message
            .get("id")
            .filter(|_| message.get("result").is_none() && message.get("error").is_none())
            .map(|id| error(id.clone(), -32600, "invalid request"));
    };
    let id = message.get("id").filter(|id| !id.is_null())?.clone();
    let result = match method {
        "initialize" => {
            let asked = message
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let version = PROTOCOL_VERSIONS
                .iter()
                .find(|known| **known == asked)
                .unwrap_or(&PROTOCOL_VERSIONS[0]);
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "agentos", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let Some(name) = message.pointer("/params/name").and_then(Value::as_str) else {
                return Some(error(id, -32602, "tools/call needs a tool name"));
            };
            let empty = Map::new();
            let arguments = match message.pointer("/params/arguments") {
                None | Some(Value::Null) => &empty,
                Some(Value::Object(arguments)) => arguments,
                Some(_) => return Some(error(id, -32602, "arguments must be an object")),
            };
            call_tool(client, name, arguments).await
        }
        _ => return Some(error(id, -32601, "method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Serve until standard input closes. Standard output carries only protocol
/// messages; anything for a person goes to standard error.
pub async fn serve(client: Client) -> io::Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) if message.is_object() => respond(&client, &message).await,
            Ok(_) => Some(error(Value::Null, -32600, "invalid request")),
            Err(_) => Some(error(Value::Null, -32700, "parse error")),
        };
        if let Some(reply) = reply {
            let mut text = reply.to_string();
            text.push('\n');
            out.write_all(text.as_bytes()).await?;
            out.flush().await?;
        }
    }
    Ok(())
}
