//! Local socket API for graphical shells.
//!
//! `agentosd --listen <path>` accepts clients on a Unix socket. Both directions
//! carry one JSON object per line.
//!
//! Client → daemon:
//! - `{"type":"intent","text":"记一下 买牛奶"}`: a new intent (or the answer to a
//!   pending confirmation, exactly like a typed line).
//! - `{"type":"confirm","approved":true}`: answer the pending confirmation.
//!
//! Daemon → client:
//! - `hello`: sent once on connect (planner label, version).
//! - `activity`: recent capability calls and confirmations from the audit log.
//! - `expression`: everything the butler says (`kind`: plan/report/ask_confirm/error).
//! - `plan`, `step`, `confirm`, `done`: structured progress, so a shell can draw
//!   the plan with risk levels instead of parsing text.
//!
//! The socket is created with mode 0600: only the session's user can talk to
//! the butler, and every request still goes through the policy engine.

use std::path::{Path, PathBuf};

use agentos_audit::{AuditEvent, AuditLog};
use agentos_core::{
    now_ms, DeviceId, DeviceInfo, Expression, ExpressionKind, Modality, Output, OutputError,
    Percept, PerceptKind, PrivacyClass, RiskLevel,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

/// Fan-out of structured events to every connected shell.
pub type EventBus = broadcast::Sender<Value>;

pub fn event_bus() -> EventBus {
    broadcast::channel(256).0
}

/// `L0`..`L3`, as shown to the human.
pub fn risk_label(r: RiskLevel) -> &'static str {
    match r {
        RiskLevel::ReadOnly => "L0",
        RiskLevel::Reversible => "L1",
        RiskLevel::External => "L2",
        RiskLevel::Irreversible => "L3",
    }
}

fn kind_label(k: &ExpressionKind) -> &'static str {
    match k {
        ExpressionKind::Plan => "plan",
        ExpressionKind::Report => "report",
        ExpressionKind::AskConfirm => "ask_confirm",
        ExpressionKind::Error => "error",
    }
}

/// An output device that forwards expressions to connected shells.
pub struct SocketOutput {
    id: DeviceId,
    bus: EventBus,
}

impl SocketOutput {
    pub fn new(bus: EventBus) -> Self {
        Self {
            id: DeviceId::new("shell"),
            bus,
        }
    }
}

#[async_trait]
impl Output for SocketOutput {
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            id: self.id.clone(),
            kind: "shell".into(),
            location: None,
            privacy: PrivacyClass::Personal,
        }
    }

    fn modalities(&self) -> &[Modality] {
        &[Modality::Visual]
    }

    async fn express(&self, e: &Expression) -> Result<(), OutputError> {
        // No subscribers is fine: nobody is looking at the screen.
        let _ = self.bus.send(json!({
            "type": "expression",
            "kind": kind_label(&e.kind),
            "intent": e.intent,
            "from": e.from.to_string(),
            "text": e.text,
        }));
        Ok(())
    }
}

/// The last `n` capability calls and confirmations, oldest first.
pub fn recent_activity(audit: &Path, n: usize) -> Vec<Value> {
    let Ok(entries) = AuditLog::replay(audit) else {
        return Vec::new();
    };
    let mut items: Vec<Value> = entries
        .into_iter()
        .filter_map(|e| match e.event {
            AuditEvent::CapabilityInvoked { capability, ok, .. } => Some(json!({
                "seq": e.seq,
                "at_ms": e.at_ms,
                "kind": "invoked",
                "capability": capability.to_string(),
                "ok": ok,
            })),
            AuditEvent::HumanConfirmation {
                capability,
                approved,
                ..
            } => Some(json!({
                "seq": e.seq,
                "at_ms": e.at_ms,
                "kind": "confirmation",
                "capability": capability.to_string(),
                "ok": approved,
            })),
            _ => None,
        })
        .collect();
    let skip = items.len().saturating_sub(n);
    items.drain(..skip);
    items
}

/// Turn one request line into the text of an intent.
pub fn parse_request(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    match v.get("type")?.as_str()? {
        "intent" => {
            let t = v.get("text")?.as_str()?.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        "confirm" => {
            let approved = v.get("approved")?.as_bool()?;
            Some(if approved { "y" } else { "取消" }.to_string())
        }
        _ => None,
    }
}

pub struct ShellServer {
    pub hello: Value,
    pub bus: EventBus,
    pub audit: PathBuf,
    pub percepts: mpsc::Sender<Percept>,
}

/// Bind `path` (replacing a stale socket) with mode 0600.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

impl ShellServer {
    pub async fn serve(self, listener: UnixListener) {
        let this = std::sync::Arc::new(self);
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let this = this.clone();
                    tokio::spawn(async move { this.client(stream).await });
                }
                Err(e) => {
                    eprintln!("shell socket accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    }

    async fn client(&self, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        // Subscribe before greeting so nothing said meanwhile is lost.
        let mut events = self.bus.subscribe();
        let greeting = [
            self.hello.clone(),
            json!({"type": "activity", "items": recent_activity(&self.audit, 20)}),
        ];
        let writer = tokio::spawn(async move {
            for v in greeting {
                if write_line(&mut write, &v).await.is_err() {
                    return;
                }
            }
            loop {
                match events.recv().await {
                    Ok(v) => {
                        if write_line(&mut write, &v).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });

        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Some(text) = parse_request(&line) else {
                continue;
            };
            let p = Percept {
                device: DeviceId::new("shell"),
                at_ms: now_ms(),
                kind: PerceptKind::Text {
                    text,
                    speaker: None,
                },
            };
            if self.percepts.send(p).await.is_err() {
                break;
            }
        }
        writer.abort();
    }
}

async fn write_line(w: &mut tokio::net::unix::OwnedWriteHalf, v: &Value) -> std::io::Result<()> {
    let mut s = v.to_string();
    s.push('\n');
    w.write_all(s.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_become_intent_text() {
        assert_eq!(
            parse_request(r#"{"type":"intent","text":" 看看笔记 "}"#).as_deref(),
            Some("看看笔记")
        );
        assert_eq!(
            parse_request(r#"{"type":"confirm","approved":true}"#).as_deref(),
            Some("y")
        );
        assert_eq!(
            parse_request(r#"{"type":"confirm","approved":false}"#).as_deref(),
            Some("取消")
        );
        assert_eq!(parse_request(r#"{"type":"intent","text":"  "}"#), None);
        assert_eq!(parse_request("not json"), None);
        assert_eq!(parse_request(r#"{"type":"shutdown"}"#), None);
    }
}
