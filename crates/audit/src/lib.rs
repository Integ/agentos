//! Append-only audit log.
//!
//! Every intent, plan, policy decision, capability call and expression is
//! written as one JSON object per line. The log is opened in append mode and
//! never rewritten, so it can be replayed to reconstruct what an agent did and why.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
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
    /// A task (one intent's work) changed state: running, waiting, done, cancelled.
    TaskState {
        task: u64,
        intent: IntentId,
        state: String,
    },
    /// The judge reviewed a step the policy engine allowed.
    JudgeVerdict {
        intent: IntentId,
        agent: AgentId,
        capability: CapabilityName,
        verdict: String,
    },
    Expressed {
        agent: AgentId,
        intent: Option<IntentId>,
        kind: String,
        text: String,
    },
    /// Someone tried to prove who they are (for example a PIN for an L3
    /// confirmation). Never contains the secret itself.
    IdentityCheck {
        method: String,
        task: Option<u64>,
        ok: bool,
        detail: String,
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
    ///
    /// If the last write was cut short (power loss, crash) the file ends in a
    /// partial line; a newline is added first so new entries stay on lines of
    /// their own and only that one partial entry is lost.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let next_seq = match File::open(&path) {
            Ok(f) => BufReader::new(f).split(b'\n').count() as u64,
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)?;
        if file.metadata()?.len() > 0 {
            let mut last = [0u8; 1];
            file.seek(SeekFrom::End(-1))?;
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                file.write_all(b"\n")?;
                file.flush()?;
            }
        }
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

    /// Read every entry back, in order. Fails on the first line that is not a
    /// valid entry; use [`AuditLog::replay_lenient`] to read past damage.
    pub fn replay(path: impl AsRef<Path>) -> io::Result<Vec<AuditEntry>> {
        let f = File::open(path)?;
        BufReader::new(f)
            .lines()
            .map(|l| serde_json::from_str(&l?).map_err(io::Error::other))
            .collect()
    }

    /// Read every valid entry back, in order, skipping lines that cannot be
    /// parsed (a write cut short by a crash, an entry type from a newer
    /// version). Returns the entries and how many lines were skipped.
    pub fn replay_lenient(path: impl AsRef<Path>) -> io::Result<(Vec<AuditEntry>, usize)> {
        let f = File::open(path)?;
        let mut entries = Vec::new();
        let mut skipped = 0;
        for line in BufReader::new(f).split(b'\n') {
            let line = line?;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice(&line) {
                Ok(e) => entries.push(e),
                Err(_) => skipped += 1,
            }
        }
        Ok((entries, skipped))
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
    fn torn_write_loses_only_the_partial_entry() {
        let path = temp_path("torn");
        let ev = AuditEvent::HumanConfirmation {
            intent: IntentId(3),
            capability: CapabilityName::new("notes.clear"),
            approved: true,
        };
        AuditLog::open(&path).unwrap().append(ev.clone()).unwrap();
        // Power cut in the middle of the second entry: no trailing newline.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"seq":1,"at_ms":5,"event":"human_conf"#)
            .unwrap();
        drop(f);

        let after = AuditLog::open(&path).unwrap().append(ev.clone()).unwrap();
        assert_eq!(after.seq, 2, "sequence numbers never go backwards");

        assert!(AuditLog::replay(&path).is_err());
        let (entries, skipped) = AuditLog::replay_lenient(&path).unwrap();
        assert_eq!(skipped, 1);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].seq, 2);
        assert_eq!(entries[1].event, ev);
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
