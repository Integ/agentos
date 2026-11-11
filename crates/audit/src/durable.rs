//! Append-only audit log.
//!
//! Every intent, plan, policy decision, capability call and expression is
//! written as one JSON object per line. The log is opened in append mode and
//! never rewritten, so it can be replayed to reconstruct what an agent did and why.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agentos_core::{now_ms, AgentId, CapabilityName, Expression, Intent, IntentId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AuditEvent {
    IntentReceived {
        intent: Box<IntentRecord>,
    },
    TaskStateChanged {
        intent: IntentId,
        state: String,
        message: Option<String>,
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
    /// Durable write-ahead marker. A start without a matching result means the
    /// capability may have run, so recovery must not blindly retry it.
    CapabilityStarted {
        intent: IntentId,
        agent: AgentId,
        capability: CapabilityName,
        args: Value,
    },
    CapabilityInvoked {
        intent: IntentId,
        agent: AgentId,
        capability: CapabilityName,
        args: Value,
        ok: bool,
        result: Value,
    },
    Expressed {
        agent: AgentId,
        intent: Option<IntentId>,
        kind: String,
        text: String,
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
    inner: Mutex<LogState>,
}

struct LogState {
    file: File,
    next_seq: u64,
    needs_newline: bool,
    failed: bool,
}

/// Validate as we read, including the final unterminated line. An otherwise
/// valid final JSON object need not end in a newline, but a partial object is
/// corruption and is never silently discarded.
fn read_verified(
    mut reader: impl BufRead,
    mut visit: impl FnMut(AuditEntry),
) -> io::Result<(u64, bool)> {
    let mut next_seq = 0;
    let mut line = Vec::new();
    let mut needs_newline = false;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let entry: AuditEntry = serde_json::from_slice(&line).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid audit record at line {}: {err}", next_seq + 1),
            )
        })?;
        if entry.seq != next_seq {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid audit sequence at line {}: expected {next_seq}, found {}",
                    next_seq + 1,
                    entry.seq
                ),
            ));
        }
        next_seq = next_seq.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "audit sequence exhausted")
        })?;
        needs_newline = !line.ends_with(b"\n");
        visit(entry);
    }
    Ok((next_seq, needs_newline))
}

#[cfg(unix)]
fn lock_exclusive(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::raw::c_int;

    extern "C" {
        fn flock(fd: c_int, operation: c_int) -> c_int;
    }

    // SAFETY: `file` owns a live descriptor. flock retains no pointer, and the
    // descriptor remains open in LogState for the lifetime of this writer.
    if unsafe { flock(file.as_raw_fd(), 2 | 4) } != 0 {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            "audit log is already locked by another writer",
        ));
    }
    Ok(())
}

impl AuditLog {
    /// Open (or create) the log at `path`. Existing records must be valid and
    /// numbered contiguously from zero. Corrupt logs are left untouched. On
    /// Unix, regular files have an exclusive writer lock until this is dropped.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        #[cfg(unix)]
        if file.metadata()?.is_file() {
            lock_exclusive(&file)?;
        }
        // Validate the locked inode, avoiding a separate path open before
        // locking. The cloned descriptor's shared read offset cannot affect
        // writes because the retained descriptor is opened in append mode.
        let (next_seq, needs_newline) = read_verified(BufReader::new(file.try_clone()?), |_| {})?;
        Ok(Self {
            path,
            inner: Mutex::new(LogState {
                file,
                next_seq,
                needs_newline,
                failed: false,
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, event: AuditEvent) -> io::Result<AuditEntry> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if guard.failed {
            return Err(io::Error::other(
                "audit log is unavailable after a previous write failure",
            ));
        }
        let entry = AuditEntry {
            seq: guard.next_seq,
            at_ms: now_ms(),
            event,
        };
        let next_seq = guard.next_seq.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "audit sequence exhausted")
        })?;
        let mut line = serde_json::to_string(&entry).map_err(io::Error::other)?;
        line.push('\n');
        if guard.needs_newline {
            line.insert(0, '\n');
        }
        let write = guard
            .file
            .write_all(line.as_bytes())
            .and_then(|()| guard.file.sync_data());
        if let Err(err) = write {
            // A failed write can leave a partial record. Keep this instance
            // unavailable rather than append more data to a corrupt tail.
            guard.failed = true;
            return Err(err);
        }
        guard.next_seq = next_seq;
        guard.needs_newline = false;
        Ok(entry)
    }

    /// Read every entry back, in order.
    pub fn replay(path: impl AsRef<Path>) -> io::Result<Vec<AuditEntry>> {
        let f = File::open(path)?;
        let mut entries = Vec::new();
        read_verified(BufReader::new(f), |entry| entries.push(entry))?;
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_path(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "agentos-task-audit-{name}-{}-{}.jsonl",
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

    fn event() -> AuditEvent {
        AuditEvent::HumanConfirmation {
            intent: IntentId(2),
            capability: CapabilityName::new("notes.clear"),
            approved: false,
        }
    }

    fn entry_line(seq: u64) -> String {
        serde_json::to_string(&AuditEntry {
            seq,
            at_ms: 123,
            event: event(),
        })
        .unwrap()
    }

    #[test]
    fn corrupt_records_are_rejected_without_changing_the_file() {
        let cases = [
            ("partial", format!("{}\n{{\"seq\":1", entry_line(0))),
            ("invalid", format!("{}\nnot json\n", entry_line(0))),
            ("blank", format!("{}\n\n", entry_line(0))),
            (
                "duplicate",
                format!("{}\n{}\n", entry_line(0), entry_line(0)),
            ),
            ("gap", format!("{}\n{}\n", entry_line(0), entry_line(2))),
            ("wrong-first", format!("{}\n", entry_line(1))),
        ];
        for (name, content) in cases {
            let path = temp_path(name);
            std::fs::write(&path, &content).unwrap();
            let err = AuditLog::open(&path).err().expect("corruption accepted");
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{name}: {err}");
            assert_eq!(
                AuditLog::replay(&path).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "{name}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn missing_final_newline_is_added_only_when_appending() {
        let path = temp_path("no-final-newline");
        let content = entry_line(0);
        std::fs::write(&path, &content).unwrap();
        let log = AuditLog::open(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        assert_eq!(AuditLog::replay(&path).unwrap().len(), 1);
        assert_eq!(log.append(event()).unwrap().seq, 1);
        let entries = AuditLog::replay(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].seq, 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn write_failure_disables_further_appends() {
        let path = temp_path("write-failure");
        std::fs::write(&path, "").unwrap();
        // A read-only descriptor gives a deterministic write failure on all
        // platforms, without relying on special devices such as /dev/full.
        let log = AuditLog {
            path: path.clone(),
            inner: Mutex::new(LogState {
                file: File::open(&path).unwrap(),
                next_seq: 0,
                needs_newline: false,
                failed: false,
            }),
        };
        assert!(log.append(event()).is_err());
        assert_eq!(log.inner.lock().unwrap().next_seq, 0);
        assert!(log.inner.lock().unwrap().failed);
        assert!(log.append(event()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn new_events_and_legacy_records_replay_together() {
        let path = temp_path("event-compatibility");
        // Existing JSON Lines retain their original shape.
        std::fs::write(&path, format!("{}\n", entry_line(0))).unwrap();
        let log = AuditLog::open(&path).unwrap();
        let started = AuditEvent::CapabilityStarted {
            intent: IntentId(2),
            agent: AgentId::new("butler"),
            capability: CapabilityName::new("notes.clear"),
            args: json!({}),
        };
        let changed = AuditEvent::TaskStateChanged {
            intent: IntentId(2),
            state: "running".into(),
            message: None,
        };
        log.append(started.clone()).unwrap();
        log.append(changed.clone()).unwrap();
        let entries = AuditLog::replay(&path).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].event, event());
        assert_eq!(entries[1].event, started);
        assert_eq!(entries[2].event, changed);
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn writer_lock_is_exclusive_but_does_not_block_replay_and_releases_on_drop() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("writer-lock");
        let first = AuditLog::open(&path).unwrap();
        first.append(event()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
            0
        );
        let error = AuditLog::open(&path).err().expect("second writer accepted");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(AuditLog::replay(&path).unwrap().len(), 1);
        drop(first);

        let second = AuditLog::open(&path).unwrap();
        assert_eq!(second.append(event()).unwrap().seq, 1);
        assert_eq!(AuditLog::replay(&path).unwrap().len(), 2);
        std::fs::remove_file(path).unwrap();
    }
}
