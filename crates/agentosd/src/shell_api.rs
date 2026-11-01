//! Local socket API for graphical shells.
//!
//! `agentosd --listen <path>` accepts clients on a Unix socket. Both directions
//! carry one JSON object per line.
//!
//! Client → daemon:
//! - `{"type":"intent","text":"记一下 买牛奶"}`: a new intent (or the answer to a
//!   pending confirmation, exactly like a typed line).
//! - `{"type":"confirm","approved":true,"task":3}`: answer a pending confirmation
//!   (`task` optional: without it, the newest waiting task).
//! - `{"type":"listen","action":"start|stop|cancel"}`: push-to-talk button.
//! - `{"type":"speech","mode":"auto|always|off"}`: when replies are read aloud.
//! - `{"type":"speech","action":"stop"}`: stop reading aloud.
//!
//! Daemon → client:
//! - `hello`: sent once on connect (planner label, version).
//! - `activity`: recent capability calls and confirmations from the audit log.
//! - `expression`: everything the butler says (`kind`: plan/report/ask_confirm/error).
//! - `plan`, `step`, `confirm`, `done`: structured progress, so a shell can draw
//!   the plan with risk levels instead of parsing text.
//! - `tainted`: a task read untrusted outside content; its side effects now
//!   need confirmation.
//! - `voice`, `voice_level`, `transcript`, `voice_notice`: microphone and
//!   speaker state, so the shell can show when the microphone is in use.
//!
//! The socket is created with mode 0600: only the session's user can talk to
//! the butler, and every request still goes through the policy engine.

use std::path::{Path, PathBuf};
use std::sync::Arc;

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

use crate::voice::{SpeakMode, Voice, VoiceControl};

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
    // A damaged line (crash mid-write) must not blank the activity panel.
    let Ok((entries, _skipped)) = AuditLog::replay_lenient(audit) else {
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
            AuditEvent::DeviceUsed { device, detail, .. } => Some(json!({
                "seq": e.seq,
                "at_ms": e.at_ms,
                "kind": "device",
                "device": device.to_string(),
                "detail": detail,
                "ok": true,
            })),
            _ => None,
        })
        .collect();
    let skip = items.len().saturating_sub(n);
    items.drain(..skip);
    items
}

/// A request from a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Text for the butler: a new intent or a confirmation answer.
    Intent(String),
    Voice(VoiceControl),
}

/// Parse one request line.
pub fn parse_request(line: &str) -> Option<Request> {
    let v: Value = serde_json::from_str(line).ok()?;
    match v.get("type")?.as_str()? {
        "intent" => {
            let t = v.get("text")?.as_str()?.trim();
            (!t.is_empty()).then(|| Request::Intent(t.to_string()))
        }
        "confirm" => {
            let approved = v.get("approved")?.as_bool()?;
            let word = if approved { "y" } else { "取消" };
            // Several tasks may be waiting; the sheet answers the one it shows.
            Some(Request::Intent(
                match v.get("task").and_then(Value::as_u64) {
                    Some(t) => format!("{word} #{t}"),
                    None => word.to_string(),
                },
            ))
        }
        "listen" => Some(Request::Voice(match v.get("action")?.as_str()? {
            "start" => VoiceControl::ListenStart,
            "stop" => VoiceControl::ListenStop,
            "cancel" => VoiceControl::ListenCancel,
            _ => return None,
        })),
        "speech" => {
            if v.get("action").and_then(Value::as_str) == Some("stop") {
                return Some(Request::Voice(VoiceControl::StopSpeaking));
            }
            let mode = SpeakMode::parse(v.get("mode")?.as_str()?)?;
            Some(Request::Voice(VoiceControl::SetSpeakMode(mode)))
        }
        _ => None,
    }
}

pub struct ShellServer {
    pub hello: Value,
    pub bus: EventBus,
    pub audit: PathBuf,
    pub percepts: mpsc::Sender<Percept>,
    /// Microphone and speaker, if configured.
    pub voice: Option<Arc<Voice>>,
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
        let this = Arc::new(self);
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

        let mut reader = BufReader::new(read);
        let mut talking = false;
        while let Ok(Some(line)) = read_request_line(&mut reader).await {
            let text = match parse_request(&line) {
                Some(Request::Intent(text)) => text,
                Some(Request::Voice(c)) => {
                    if let Some(voice) = &self.voice {
                        match c {
                            VoiceControl::ListenStart => talking = true,
                            VoiceControl::ListenStop | VoiceControl::ListenCancel => {
                                talking = false
                            }
                            _ => {}
                        }
                        voice.control(c).await;
                    }
                    continue;
                }
                None => continue,
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
        // A shell that disappears mid-recording must not leave the mic open.
        if let (true, Some(voice)) = (talking, &self.voice) {
            voice.control(VoiceControl::ListenCancel).await;
        }
        writer.abort();
    }
}

/// Longest request line a shell may send. Intents are short; this only stops
/// a broken client from making the daemon buffer without limit.
pub const MAX_REQUEST_LINE: usize = 64 * 1024;

/// Read one request line. `Ok(None)` at end of stream; an error for a line
/// over [`MAX_REQUEST_LINE`] or invalid UTF-8, which ends the connection.
pub async fn read_request_line<R: tokio::io::AsyncBufRead + Unpin>(
    r: &mut R,
) -> std::io::Result<Option<String>> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let n = (&mut *r)
        .take(MAX_REQUEST_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    } else if buf.len() > MAX_REQUEST_LINE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "request line too long",
        ));
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
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
        let text = |l: &str| match parse_request(l) {
            Some(Request::Intent(t)) => Some(t),
            _ => None,
        };
        assert_eq!(
            text(r#"{"type":"intent","text":" 看看笔记 "}"#).as_deref(),
            Some("看看笔记")
        );
        assert_eq!(
            text(r#"{"type":"confirm","approved":true}"#).as_deref(),
            Some("y")
        );
        assert_eq!(
            text(r#"{"type":"confirm","approved":false}"#).as_deref(),
            Some("取消")
        );
        assert_eq!(
            text(r#"{"type":"confirm","approved":true,"task":3}"#).as_deref(),
            Some("y #3")
        );
        assert_eq!(parse_request(r#"{"type":"intent","text":"  "}"#), None);
        assert_eq!(parse_request("not json"), None);
        assert_eq!(parse_request(r#"{"type":"shutdown"}"#), None);
    }

    #[tokio::test]
    async fn request_lines_are_capped() {
        let mut ok = BufReader::new(&b"{\"a\":1}\r\nlast"[..]);
        assert_eq!(
            read_request_line(&mut ok).await.unwrap().as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(
            read_request_line(&mut ok).await.unwrap().as_deref(),
            Some("last")
        );
        assert_eq!(read_request_line(&mut ok).await.unwrap(), None);

        let exact = format!("{}\n", "x".repeat(MAX_REQUEST_LINE));
        let mut r = BufReader::new(exact.as_bytes());
        assert_eq!(
            read_request_line(&mut r).await.unwrap().map(|l| l.len()),
            Some(MAX_REQUEST_LINE)
        );

        let huge = "x".repeat(MAX_REQUEST_LINE + 10);
        let mut r = BufReader::new(huge.as_bytes());
        assert!(read_request_line(&mut r).await.is_err());
    }

    #[test]
    fn voice_requests() {
        let v = |l: &str| match parse_request(l) {
            Some(Request::Voice(c)) => Some(c),
            _ => None,
        };
        assert_eq!(
            v(r#"{"type":"listen","action":"start"}"#),
            Some(VoiceControl::ListenStart)
        );
        assert_eq!(
            v(r#"{"type":"listen","action":"stop"}"#),
            Some(VoiceControl::ListenStop)
        );
        assert_eq!(
            v(r#"{"type":"listen","action":"cancel"}"#),
            Some(VoiceControl::ListenCancel)
        );
        assert_eq!(v(r#"{"type":"listen","action":"forever"}"#), None);
        assert_eq!(
            v(r#"{"type":"speech","mode":"always"}"#),
            Some(VoiceControl::SetSpeakMode(SpeakMode::Always))
        );
        assert_eq!(
            v(r#"{"type":"speech","action":"stop"}"#),
            Some(VoiceControl::StopSpeaking)
        );
        assert_eq!(v(r#"{"type":"speech","mode":"loud"}"#), None);
    }
}
