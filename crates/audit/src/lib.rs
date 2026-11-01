//! Append-only audit log.
//!
//! Every intent, plan, policy decision, capability call and expression is
//! written as one JSON object per line. The log is opened in append mode and
//! never rewritten, so it can be replayed to reconstruct what an agent did and why.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agentos_core::{now_ms, AgentId, CapabilityName, DeviceId, Expression, Intent, IntentId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AuditEvent {
    IntentReceived {
        intent: Box<IntentRecord>,
    },
    PlanCreated {
        intent: IntentId,
        agent: AgentId,
        steps: Vec<String>,
    },
    PolicyDecision {
        intent: IntentId,
        agent: AgentId,
        capability: CapabilityName,
        decision: String,
    },
    HumanConfirmation {
        intent: IntentId,
        capability: CapabilityName,
        approved: bool,
    },
    CapabilityInvoked {
        intent: IntentId,
        agent: AgentId,
        capability: CapabilityName,
        args: Value,
        ok: bool,
        result: Value,
    },
    /// An agent handed part of an intent to another agent.
    Delegated {
        intent: IntentId,
        from: AgentId,
        to: AgentId,
        task: String,
        /// Capabilities the delegate may use for this task.
        capabilities: Vec<CapabilityName>,
    },
    /// A delegated agent finished its task (or gave up).
    AgentFinished {
        intent: IntentId,
        agent: AgentId,
        ok: bool,
        summary: String,
    },
    Expressed {
        agent: AgentId,
        intent: Option<IntentId>,
        kind: String,
        text: String,
    },
    /// A privacy-relevant device was used, e.g. the microphone recorded
    /// speech. `detail` says how long and where the raw data went.
    DeviceUsed {
        device: DeviceId,
        action: String,
        detail: Value,
    },
}

/// Serializable snapshot of an [`Intent`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IntentRecord {
    pub id: IntentId,
    pub source: String,
    pub speaker: Option<String>,
    pub text: String,
}

impl From<&Intent> for IntentRecord {
    fn from(i: &Intent) -> Self {
        Self {
            id: i.id,
            source: i.source.to_string(),
            speaker: i.speaker.clone(),
            text: i.text.clone(),
        }
    }
}

impl AuditEvent {
    pub fn expressed(e: &Expression) -> Self {
        AuditEvent::Expressed {
            agent: e.from.clone(),
            intent: e.intent,
            kind: format!("{:?}", e.kind).to_lowercase(),
            text: e.text.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditEntry {
    pub seq: u64,
    pub at_ms: u64,
    #[serde(flatten)]
    pub event: AuditEvent,
}

pub struct AuditLog {
    path: PathBuf,
    inner: Mutex<(File, u64)>,
}

impl AuditLog {
    /// Open (or create) the log at `path`. Sequence numbers continue from the
    /// entries already present.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let next_seq = match File::open(&path) {
            Ok(f) => BufReader::new(f).lines().count() as u64,
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            inner: Mutex::new((file, next_seq)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, event: AuditEvent) -> io::Result<AuditEntry> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let (file, seq) = &mut *guard;
        let entry = AuditEntry {
            seq: *seq,
            at_ms: now_ms(),
            event,
        };
        let mut line = serde_json::to_string(&entry).map_err(io::Error::other)?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        file.flush()?;
        *seq += 1;
        Ok(entry)
    }

    /// Read every entry back, in order.
    pub fn replay(path: impl AsRef<Path>) -> io::Result<Vec<AuditEntry>> {
        let f = File::open(path)?;
        BufReader::new(f)
            .lines()
            .map(|l| serde_json::from_str(&l?).map_err(io::Error::other))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_path(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "agentos-audit-{name}-{}-{}.jsonl",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn append_and_replay_round_trip() {
        let path = temp_path("roundtrip");
        let log = AuditLog::open(&path).unwrap();
        let ev = AuditEvent::CapabilityInvoked {
            intent: IntentId(1),
            agent: AgentId::new("butler"),
            capability: CapabilityName::new("clock.now"),
            args: json!({}),
            ok: true,
            result: json!({"unix_ms": 1}),
        };
        log.append(ev.clone()).unwrap();
        let entries = AuditLog::replay(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[0].event, ev);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reopen_continues_sequence() {
        let path = temp_path("reopen");
        let ev = AuditEvent::HumanConfirmation {
            intent: IntentId(2),
            capability: CapabilityName::new("notes.clear"),
            approved: false,
        };
        AuditLog::open(&path).unwrap().append(ev.clone()).unwrap();
        let second = AuditLog::open(&path).unwrap().append(ev).unwrap();
        assert_eq!(second.seq, 1);
        std::fs::remove_file(&path).unwrap();
    }
}
