//! Append-only audit log.
//!
//! Every intent, plan, policy decision, capability call and expression is
//! written as one JSON object per line. The log is opened in append mode and
//! never rewritten, so it can be replayed to reconstruct what an agent did and why.
//!
//! Entries are hash-chained: each carries `prev`, the SHA-256 of the previous
//! line exactly as written. Editing or deleting an entry in the middle breaks
//! the chain at the next one, which [`AuditLog::verify`] reports. (Rewriting
//! every later entry, or cutting off the tail, is not detectable from the
//! file alone; note [`VerifyReport::head`] somewhere else to catch that.)

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
    /// SHA-256 (hex) of the previous line; [`GENESIS`] for the first entry.
    /// Absent in logs written before chaining was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
    #[serde(flatten)]
    pub event: AuditEvent,
}

/// `prev` of the first entry in a log.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn line_hash(line: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(line)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What [`AuditLog::verify`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Non-empty lines in the file.
    pub lines: usize,
    /// Entries carrying a `prev` link.
    pub chained: usize,
    /// Entries from before chaining (no `prev`), only allowed at the start.
    pub legacy: usize,
    /// 1-based line numbers that are not valid entries (e.g. a write cut
    /// short by a crash). They stay part of the chain.
    pub damaged: Vec<usize>,
    /// 1-based line numbers where the chain does not match: an entry before
    /// it was changed, removed or inserted.
    pub breaks: Vec<usize>,
    /// Hash of the last line: the value the next entry will link to.
    pub head: String,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.breaks.is_empty()
    }
}

pub struct AuditLog {
    path: PathBuf,
    /// The file, the next sequence number, and the hash of the last line.
    inner: Mutex<(File, u64, String)>,
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
        let (next_seq, head) = match File::open(&path) {
            Ok(f) => {
                let mut n = 0u64;
                let mut last: Option<Vec<u8>> = None;
                for line in BufReader::new(f).split(b'\n') {
                    n += 1;
                    last = Some(line?);
                }
                (
                    n,
                    last.map_or_else(|| GENESIS.to_string(), |l| line_hash(&l)),
                )
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (0, GENESIS.to_string()),
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
            inner: Mutex::new((file, next_seq, head)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, event: AuditEvent) -> io::Result<AuditEntry> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let (file, seq, head) = &mut *guard;
        let entry = AuditEntry {
            seq: *seq,
            at_ms: now_ms(),
            prev: Some(head.clone()),
            event,
        };
        let mut line = serde_json::to_string(&entry).map_err(io::Error::other)?;
        let hash = line_hash(line.as_bytes());
        line.push('\n');
        file.write_all(line.as_bytes())?;
        file.flush()?;
        *seq += 1;
        *head = hash;
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

    /// Check the hash chain of the log at `path`.
    pub fn verify(path: impl AsRef<Path>) -> io::Result<VerifyReport> {
        let f = File::open(path)?;
        let mut r = VerifyReport {
            head: GENESIS.to_string(),
            ..VerifyReport::default()
        };
        // Hash of the previous line, and whether chaining has started.
        let mut prev_hash = GENESIS.to_string();
        let mut started = false;
        for (i, line) in BufReader::new(f).split(b'\n').enumerate() {
            let line = line?;
            let no = i + 1;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            r.lines += 1;
            match serde_json::from_slice::<AuditEntry>(&line) {
                Err(_) => r.damaged.push(no),
                Ok(e) => match e.prev {
                    Some(p) => {
                        r.chained += 1;
                        // The first link may follow legacy entries; after that
                        // every link must match the line before it.
                        if p != prev_hash {
                            r.breaks.push(no);
                        }
                        started = true;
                    }
                    None if started => r.breaks.push(no),
                    None => r.legacy += 1,
                },
            }
            prev_hash = line_hash(&line);
        }
        r.head = prev_hash;
        Ok(r)
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

    fn sample(n: u64) -> AuditEvent {
        AuditEvent::HumanConfirmation {
            intent: IntentId(n),
            capability: CapabilityName::new("notes.clear"),
            approved: n % 2 == 0,
        }
    }

    #[test]
    fn entries_are_hash_chained_across_reopen() {
        let path = temp_path("chain");
        let log = AuditLog::open(&path).unwrap();
        let first = log.append(sample(1)).unwrap();
        assert_eq!(first.prev.as_deref(), Some(GENESIS));
        log.append(sample(2)).unwrap();
        drop(log);
        AuditLog::open(&path).unwrap().append(sample(3)).unwrap();
        let r = AuditLog::verify(&path).unwrap();
        assert!(r.ok(), "{r:?}");
        assert_eq!((r.lines, r.chained, r.legacy), (3, 3, 0));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(r.head, line_hash(text.lines().last().unwrap().as_bytes()));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn editing_or_deleting_an_entry_breaks_the_chain() {
        let path = temp_path("tamper");
        let log = AuditLog::open(&path).unwrap();
        for n in 0..4 {
            log.append(sample(n)).unwrap();
        }
        drop(log);
        let original = std::fs::read_to_string(&path).unwrap();

        // Flip one approval: the next line no longer links to it.
        let edited = original.replacen("\"approved\":false", "\"approved\":true", 1);
        assert_ne!(edited, original);
        std::fs::write(&path, &edited).unwrap();
        let r = AuditLog::verify(&path).unwrap();
        assert!(!r.ok());
        assert_eq!(r.breaks, vec![3]);

        // Delete the second line.
        let lines: Vec<&str> = original.lines().collect();
        let deleted = format!("{}\n{}\n{}\n", lines[0], lines[2], lines[3]);
        std::fs::write(&path, deleted).unwrap();
        assert_eq!(AuditLog::verify(&path).unwrap().breaks, vec![2]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn legacy_entries_and_torn_writes_are_not_tampering() {
        let path = temp_path("legacy");
        // Two entries from before chaining.
        let old = |seq: u64| {
            format!(
                "{{\"seq\":{seq},\"at_ms\":1,\"event\":\"human_confirmation\",\"intent\":1,\"capability\":\"x\",\"approved\":true}}\n"
            )
        };
        std::fs::write(&path, old(0) + &old(1)).unwrap();
        let log = AuditLog::open(&path).unwrap();
        log.append(sample(2)).unwrap();
        drop(log);
        // A crash mid-write, then more entries.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"seq":3,"at_"#).unwrap();
        drop(f);
        AuditLog::open(&path).unwrap().append(sample(4)).unwrap();
        let r = AuditLog::verify(&path).unwrap();
        assert!(r.ok(), "{r:?}");
        assert_eq!((r.legacy, r.chained), (2, 2));
        assert_eq!(r.damaged, vec![4]);

        // But an unchained entry after chaining started is a break.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(old(6).as_bytes()).unwrap();
        drop(f);
        assert_eq!(AuditLog::verify(&path).unwrap().breaks, vec![6]);
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
