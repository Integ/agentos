//! Append-only audit log.
//!
//! Every intent, plan, policy decision, capability call and expression is
//! written as one JSON object per line. The log is opened in append mode and
//! never rewritten, so it can be replayed to reconstruct what an agent did and why.
//!
//! **Durable.** Each record is synced to disk before `append` returns, and a
//! log has one writer at a time (an exclusive lock on Unix). A failed write
//! disables the writer rather than risk appending to a corrupt tail.
//!
//! **Hash-chained.** Each entry carries `prev`, the SHA-256 of the previous
//! line exactly as written. Editing or deleting an entry in the middle breaks
//! the chain at the next one. (Rewriting every later entry, or cutting off the
//! tail, is not detectable from the file alone; note [`VerifyReport::head`]
//! somewhere else to catch that.)
//!
//! **Strict.** A log is only continued if every record is valid, numbered
//! contiguously, and the chain is intact. The single tolerated damage is the
//! one a crash produces: a final line cut short. It is sealed with a newline,
//! keeps its sequence number, and stays part of the chain, so the entry after
//! it vouches for it. Anything else is refused and the file is left untouched;
//! a daemon may then set it aside with [`AuditLog::open_or_set_aside`].

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
    /// A durable task changed state; `message` is what the task says about it.
    TaskStateChanged {
        intent: IntentId,
        state: String,
        message: Option<String>,
    },
    /// A task of the legacy `agentosd` daemon changed state: running, waiting,
    /// done, unhandled, failed, cancelled.
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
    /// The previous agentosd run crashed; `summary` is the first line of its
    /// crash report (message and source location).
    DaemonCrashed {
        summary: String,
    },
    /// The previous log could not be continued (damaged or altered) and was
    /// set aside under `previous`; this log starts a new chain.
    LogRecovered {
        previous: String,
        reason: String,
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
    inner: Mutex<LogState>,
}

struct LogState {
    file: File,
    next_seq: u64,
    /// Hash of the last line: what the next entry links to.
    head: String,
    needs_newline: bool,
    failed: bool,
}

/// Where a valid log stands after its last line.
struct Position {
    next_seq: u64,
    head: String,
    /// The file does not end in a newline (a complete final record written
    /// without one, or a final line cut short).
    needs_newline: bool,
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Validate a whole log while reading it, and hand every valid entry to
/// `visit`. See the module documentation for what is accepted.
fn scan(mut reader: impl BufRead, mut visit: impl FnMut(AuditEntry)) -> io::Result<Position> {
    let mut next_seq: u64 = 0;
    let mut head = GENESIS.to_string();
    let mut chained = false;
    // A line that did not parse: accepted only if it is the last line and
    // unterminated, or if the next entry's `prev` vouches for it.
    let mut pending_damage: Option<u64> = None;
    let mut needs_newline = false;
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let number = next_seq + 1;
        let terminated = line.ends_with(b"\n");
        let content = if terminated {
            &line[..line.len() - 1]
        } else {
            &line[..]
        };
        needs_newline = !terminated;
        match serde_json::from_slice::<AuditEntry>(content) {
            Ok(entry) => {
                if entry.seq != next_seq {
                    return Err(invalid(format!(
                        "invalid audit sequence at line {number}: expected {next_seq}, found {}",
                        entry.seq
                    )));
                }
                match &entry.prev {
                    Some(prev) if *prev != head => {
                        return Err(invalid(format!(
                            "audit hash chain is broken at line {number}: an earlier record was changed, removed or inserted"
                        )));
                    }
                    Some(_) => chained = true,
                    None if chained => {
                        return Err(invalid(format!(
                            "unchained audit record at line {number} after the chain started"
                        )));
                    }
                    // Nothing vouches for a damaged line in an unchained log.
                    None if pending_damage.is_some() => {
                        return Err(invalid(format!(
                            "invalid audit record at line {}",
                            number - 1
                        )));
                    }
                    None => {}
                }
                pending_damage = None;
                visit(entry);
            }
            Err(error) => {
                if pending_damage.is_some() || (terminated && content.is_empty()) {
                    return Err(invalid(format!(
                        "invalid audit record at line {number}: {error}"
                    )));
                }
                pending_damage = Some(number);
            }
        }
        head = line_hash(content);
        next_seq = next_seq
            .checked_add(1)
            .ok_or_else(|| invalid("audit sequence exhausted".into()))?;
    }
    if let Some(number) = pending_damage {
        // Only a final line cut short may remain unvouched.
        if !needs_newline {
            return Err(invalid(format!("invalid audit record at line {number}")));
        }
    }
    Ok(Position {
        next_seq,
        head,
        needs_newline,
    })
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
    /// Open (or create) the log at `path` and continue it. Existing records
    /// must form a valid log (see the module documentation); otherwise the
    /// file is left untouched and `InvalidData` is returned. On Unix, regular
    /// files have an exclusive writer lock until this is dropped.
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
        let position = scan(BufReader::new(file.try_clone()?), |_| {})?;
        Ok(Self {
            path,
            inner: Mutex::new(LogState {
                file,
                next_seq: position.next_seq,
                head: position.head,
                needs_newline: position.needs_newline,
                failed: false,
            }),
        })
    }

    /// Like [`AuditLog::open`], but a log that cannot be continued does not
    /// keep the system from starting: it is renamed next to itself (nothing is
    /// deleted or rewritten) and a new log begins with a record saying so.
    /// Returns where the old log went, if that happened.
    pub fn open_or_set_aside(path: impl AsRef<Path>) -> io::Result<(Self, Option<PathBuf>)> {
        let path = path.as_ref();
        let error = match Self::open(path) {
            Ok(log) => return Ok((log, None)),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => error,
            Err(error) => return Err(error),
        };
        let mut name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "audit log needs a name"))?
            .to_os_string();
        name.push(format!(".damaged-{}", now_ms()));
        let aside = path.with_file_name(name);
        std::fs::rename(path, &aside)?;
        let log = Self::open(path)?;
        log.append(AuditEvent::LogRecovered {
            previous: aside.display().to_string(),
            reason: error.to_string(),
        })?;
        Ok((log, Some(aside)))
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
            prev: Some(guard.head.clone()),
            event,
        };
        let next_seq = guard
            .next_seq
            .checked_add(1)
            .ok_or_else(|| invalid("audit sequence exhausted".into()))?;
        let mut line = serde_json::to_string(&entry).map_err(io::Error::other)?;
        let hash = line_hash(line.as_bytes());
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
        guard.head = hash;
        guard.needs_newline = false;
        Ok(entry)
    }

    /// Read every entry back, in order, validating the log exactly as
    /// [`AuditLog::open`] does. A sealed line cut short by a crash is not an
    /// entry and is not returned.
    pub fn replay(path: impl AsRef<Path>) -> io::Result<Vec<AuditEntry>> {
        let f = File::open(path)?;
        let mut entries = Vec::new();
        scan(BufReader::new(f), |entry| entries.push(entry))?;
        Ok(entries)
    }

    /// Report on the hash chain of the log at `path` without refusing it:
    /// which lines are damaged and where the chain breaks.
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
    /// version). Returns the entries and how many lines were skipped. For
    /// displays that must not go blank over a damaged log.
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

    fn sample(n: u64) -> AuditEvent {
        AuditEvent::HumanConfirmation {
            intent: IntentId(n),
            capability: CapabilityName::new("notes.clear"),
            approved: n.is_multiple_of(2),
        }
    }

    /// A record as written before chaining existed.
    fn unchained_line(seq: u64) -> String {
        format!(
            "{{\"seq\":{seq},\"at_ms\":123,\"event\":\"human_confirmation\",\"intent\":2,\"capability\":\"notes.clear\",\"approved\":false}}"
        )
    }

    fn append_raw(path: &Path, bytes: &[u8]) {
        let mut f = OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(bytes).unwrap();
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
    fn reopen_continues_sequence_and_chain() {
        let path = temp_path("reopen");
        let log = AuditLog::open(&path).unwrap();
        let first = log.append(sample(1)).unwrap();
        assert_eq!(first.prev.as_deref(), Some(GENESIS));
        log.append(sample(2)).unwrap();
        drop(log);
        let third = AuditLog::open(&path).unwrap().append(sample(3)).unwrap();
        assert_eq!(third.seq, 2);
        let r = AuditLog::verify(&path).unwrap();
        assert!(r.ok(), "{r:?}");
        assert_eq!((r.lines, r.chained, r.legacy), (3, 3, 0));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(r.head, line_hash(text.lines().last().unwrap().as_bytes()));
        assert_eq!(AuditLog::replay(&path).unwrap().len(), 3);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_final_line_cut_short_is_sealed_and_stays_in_the_chain() {
        let path = temp_path("torn");
        AuditLog::open(&path).unwrap().append(sample(3)).unwrap();
        // Power cut in the middle of the second entry: no trailing newline.
        append_raw(&path, br#"{"seq":1,"at_ms":5,"event":"human_conf"#);
        let before = std::fs::read(&path).unwrap();

        // Opening alone changes nothing; the seal is written with the next record.
        let log = AuditLog::open(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let after = log.append(sample(4)).unwrap();
        assert_eq!(after.seq, 2, "the lost record keeps its sequence number");
        drop(log);

        // The log can be reopened and replayed; the torn line is not an entry.
        let entries = AuditLog::replay(&path).unwrap();
        assert_eq!(
            entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert_eq!(
            AuditLog::open(&path)
                .unwrap()
                .append(sample(5))
                .unwrap()
                .seq,
            3
        );
        let r = AuditLog::verify(&path).unwrap();
        assert!(r.ok(), "{r:?}");
        assert_eq!(r.damaged, vec![2]);
        let (lenient, skipped) = AuditLog::replay_lenient(&path).unwrap();
        assert_eq!((lenient.len(), skipped), (3, 1));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn any_other_damage_is_refused_without_changing_the_file() {
        let good = unchained_line(0);
        let cases = [
            ("invalid", format!("{good}\nnot json\n")),
            ("blank", format!("{good}\n\n")),
            ("duplicate", format!("{good}\n{}\n", unchained_line(0))),
            ("gap", format!("{good}\n{}\n", unchained_line(2))),
            ("wrong-first", format!("{}\n", unchained_line(1))),
            // Nothing vouches for a damaged line in a log without a chain.
            (
                "unvouched",
                format!("{good}\n{{\"seq\":1\n{}\n", unchained_line(2)),
            ),
            ("two-damaged", format!("{good}\nx\ny")),
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
    fn editing_or_deleting_an_entry_breaks_the_chain_and_the_log_is_not_continued() {
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
        assert_eq!(
            AuditLog::open(&path).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);

        // Delete the second line.
        let lines: Vec<&str> = original.lines().collect();
        let deleted = format!("{}\n{}\n{}\n", lines[0], lines[2], lines[3]);
        std::fs::write(&path, deleted).unwrap();
        assert_eq!(AuditLog::verify(&path).unwrap().breaks, vec![2]);
        assert!(AuditLog::open(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn unchained_records_from_older_versions_are_continued_with_a_chain() {
        let path = temp_path("legacy");
        std::fs::write(
            &path,
            format!("{}\n{}\n", unchained_line(0), unchained_line(1)),
        )
        .unwrap();
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
        let first_chained = log.append(started.clone()).unwrap();
        assert_eq!(
            first_chained.prev.as_deref(),
            Some(line_hash(unchained_line(1).as_bytes()).as_str())
        );
        log.append(changed.clone()).unwrap();
        drop(log);
        let entries = AuditLog::replay(&path).unwrap();
        assert_eq!(entries.len(), 4);
        assert!(entries[0].prev.is_none() && entries[1].prev.is_none());
        assert_eq!(entries[2].event, started);
        assert_eq!(entries[3].event, changed);
        let r = AuditLog::verify(&path).unwrap();
        assert!(r.ok(), "{r:?}");
        assert_eq!((r.legacy, r.chained), (2, 2));

        // But an unchained entry after chaining started is a break.
        append_raw(&path, format!("{}\n", unchained_line(4)).as_bytes());
        assert_eq!(AuditLog::verify(&path).unwrap().breaks, vec![5]);
        assert!(AuditLog::open(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn missing_final_newline_is_added_only_when_appending() {
        let path = temp_path("no-final-newline");
        let content = unchained_line(0);
        std::fs::write(&path, &content).unwrap();
        let log = AuditLog::open(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        assert_eq!(AuditLog::replay(&path).unwrap().len(), 1);
        assert_eq!(log.append(sample(1)).unwrap().seq, 1);
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
                head: GENESIS.to_string(),
                needs_newline: false,
                failed: false,
            }),
        };
        assert!(log.append(sample(1)).is_err());
        assert_eq!(log.inner.lock().unwrap().next_seq, 0);
        assert!(log.inner.lock().unwrap().failed);
        assert!(log.append(sample(1)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_log_that_cannot_be_continued_is_set_aside_not_deleted() {
        let path = temp_path("set-aside");
        let (log, aside) = AuditLog::open_or_set_aside(&path).unwrap();
        assert!(aside.is_none());
        log.append(sample(1)).unwrap();
        log.append(sample(2)).unwrap();
        drop(log);
        // A healthy log is simply continued.
        let (log, aside) = AuditLog::open_or_set_aside(&path).unwrap();
        assert!(aside.is_none());
        drop(log);

        let tampered = std::fs::read_to_string(&path).unwrap().replacen(
            "\"approved\":false",
            "\"approved\":true",
            1,
        );
        std::fs::write(&path, &tampered).unwrap();
        let (log, aside) = AuditLog::open_or_set_aside(&path).unwrap();
        let aside = aside.expect("the altered log is set aside");
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), tampered);
        log.append(sample(3)).unwrap();
        drop(log);
        let entries = AuditLog::replay(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(
            &entries[0].event,
            AuditEvent::LogRecovered { previous, reason }
                if previous == &aside.display().to_string() && reason.contains("chain")
        ));
        assert_eq!(entries[0].prev.as_deref(), Some(GENESIS));
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(&aside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn writer_lock_is_exclusive_but_does_not_block_replay_and_releases_on_drop() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("writer-lock");
        let first = AuditLog::open(&path).unwrap();
        first.append(sample(1)).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
            0
        );
        let error = AuditLog::open(&path).err().expect("second writer accepted");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        // A locked log is not "damaged": it must never be set aside.
        assert_eq!(
            AuditLog::open_or_set_aside(&path).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(AuditLog::replay(&path).unwrap().len(), 1);
        drop(first);

        let second = AuditLog::open(&path).unwrap();
        assert_eq!(second.append(sample(2)).unwrap().seq, 1);
        assert_eq!(AuditLog::replay(&path).unwrap().len(), 2);
        std::fs::remove_file(path).unwrap();
    }
}
