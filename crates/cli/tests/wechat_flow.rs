//! The WeChat bridge against a fake iLink server and a real task service:
//! the bound user's words become tasks, results come back to that user, and
//! nobody else is listened to.

#![cfg(unix)]

use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentos_audit::AuditLog;
use agentos_cli::client::Client;
use agentos_cli::wechat::{self, Login};
use agentos_core::AgentId;
use agentos_planner::RulePlanner;
use agentos_policy::PolicyEngine;
use agentos_runtime::{TaskId, TaskStore};
use agentos_services::registry::CapabilityRegistry;
use agentos_taskd::builtin;
use agentos_taskd::butler::grant_manifest;
use agentos_taskd::service::Service;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// What the fake server saw: each request's path, headers and body.
#[derive(Default)]
struct Seen {
    requests: Vec<(String, String, Value)>,
    polls: usize,
}

/// A minimal iLink: the first poll delivers two messages (one from the
/// owner, one from a stranger), later polls deliver nothing.
async fn fake_ilink(seen: Arc<Mutex<Seen>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                // Read the head, then as much body as it announces.
                let (head, body) = loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let length = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        while buf.len() < end + 4 + length {
                            let n = stream.read(&mut chunk).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        break (head, buf[end + 4..].to_vec());
                    }
                };
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let reply = if path.contains("getupdates") {
                    let first = {
                        let mut s = seen.lock().unwrap();
                        s.polls += 1;
                        s.polls == 1
                    };
                    if first {
                        json!({"ret": 0, "get_updates_buf": "c1", "msgs": [
                            {"message_type": 1, "from_user_id": "owner", "message_id": 1,
                             "context_token": "ctx-1",
                             "item_list": [{"type": 1, "text_item": {"text": "记一下 买牛奶"}}]},
                            {"message_type": 1, "from_user_id": "stranger", "message_id": 2,
                             "item_list": [{"type": 1, "text_item": {"text": "清空笔记"}}]}
                        ]})
                    } else {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        json!({"ret": 0, "get_updates_buf": "c1", "msgs": []})
                    }
                } else {
                    json!({"ret": 0})
                };
                seen.lock().unwrap().requests.push((path, head, body));
                let text = reply.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn the_bound_user_talks_to_agentos_and_strangers_are_ignored() {
    let base = if cfg!(target_os = "macos") {
        "/private/tmp"
    } else {
        "/tmp"
    };
    let dir = PathBuf::from(format!(
        "{base}/aos-wx-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let tasks = Arc::new(TaskStore::memory());
    let audit = Arc::new(AuditLog::open(dir.join("audit.jsonl")).unwrap());
    let mut registry = CapabilityRegistry::new();
    let notes = builtin::register_all(&mut registry);
    let mut policy = PolicyEngine::new();
    let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
    grant_manifest(&mut policy, &AgentId::new("butler"), &names);
    let socket = dir.join("control.sock");
    let server = Service::new(
        tasks.clone(),
        audit,
        registry,
        policy,
        Box::new(RulePlanner),
    )
    .bind(&socket)
    .await
    .unwrap();
    let (_stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(server.run_until(async {
        let _ = stopped.await;
    }));

    // Logged in already, bound to nobody yet: the first writer is bound.
    Login {
        token: "tok-123".into(),
        bot_id: "bot".into(),
        owner: None,
        cursor: String::new(),
    }
    .save(&dir)
    .unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let api = fake_ilink(seen.clone()).await;
    let bridge = tokio::spawn({
        let dir = dir.clone();
        let client = Client::new(&socket);
        async move { wechat::run(client, &dir, &api).await }
    });

    let sent = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let found = seen
                .lock()
                .unwrap()
                .requests
                .iter()
                .find(|(path, _, _)| path.contains("sendmessage"))
                .cloned();
            if let Some(found) = found {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a reply was sent");
    bridge.abort();

    let (_, head, body) = sent;
    // The reply goes to the owner, in the conversation the message came from.
    assert_eq!(body["msg"]["to_user_id"], "owner");
    assert_eq!(body["msg"]["context_token"], "ctx-1");
    let text = body["msg"]["item_list"][0]["text_item"]["text"]
        .as_str()
        .unwrap();
    // What is said is the task's own outcome, word for word.
    let task = tasks.get(TaskId(1)).unwrap();
    assert_eq!(task.state, agentos_runtime::TaskState::Succeeded);
    assert_eq!(Some(text), task.message.as_deref());
    let head = head.to_ascii_lowercase();
    assert!(head.contains("authorization: bearer tok-123"), "{head}");
    assert!(
        head.contains("authorizationtype: ilink_bot_token"),
        "{head}"
    );
    // Guards the binding: only the owner's words became a task; the
    // stranger's "清空笔记" never reached AgentOS.
    assert_eq!(*notes.lock().unwrap(), vec!["买牛奶".to_string()]);
    assert!(tasks.get(TaskId(2)).is_none());
    let login = Login::load(&dir).unwrap();
    assert_eq!(login.owner.as_deref(), Some("owner"));
    assert_eq!(login.cursor, "c1");
    let _ = std::fs::remove_dir_all(&dir);
}
