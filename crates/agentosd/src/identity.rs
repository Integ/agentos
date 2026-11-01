//! Identity check for L3 (irreversible) confirmations: a local PIN.
//!
//! The policy engine marks L3 steps `verify_identity`. Without a PIN anyone at
//! the keyboard (or a script talking to the shell socket) could approve them
//! with a plain "y". With a PIN set, the butler only accepts an L3 approval
//! that a fresh, successful PIN entry vouched for.
//!
//! The PIN never travels as intent text, so it is never written to the audit
//! log: the shell sends it next to the confirmation, the daemon checks it here
//! and keeps a short-lived *grant* for that task, then forwards a plain "y".
//!
//! Stored as `{salt, rounds, hash}` (iterated SHA-256) in a 0600 file. Five
//! wrong PINs lock entry for five minutes. This keeps out a prompt injection,
//! a guest at the screen and casual brute force; it is not protection against
//! someone who already runs code as the desktop user (they can replace the
//! file). Moving the vault to a separate privileged service is future work.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentos_audit::{AuditEvent, AuditLog};
use agentos_core::now_ms;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const ROUNDS: u32 = 200_000;
const MAX_FAILURES: u32 = 5;
const LOCKOUT_MS: u64 = 5 * 60 * 1000;
/// How long a successful PIN entry vouches for the approval that follows.
const GRANT_MS: u64 = 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PinRecord {
    salt: String,
    rounds: u32,
    hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinCheck {
    Ok,
    Wrong { attempts_left: u32 },
    Locked { seconds: u64 },
    NotSet,
}

impl PinCheck {
    /// What to tell the human.
    pub fn message(&self) -> String {
        match self {
            PinCheck::Ok => "PIN 正确".into(),
            PinCheck::Wrong { attempts_left } => {
                format!("PIN 不对，还可以再试 {attempts_left} 次")
            }
            PinCheck::Locked { seconds } => {
                format!("PIN 输错太多次，请 {seconds} 秒后再试")
            }
            PinCheck::NotSet => "还没有设置 PIN（运行 agentosd --set-pin）".into(),
        }
    }
}

#[derive(Default)]
struct State {
    record: Option<PinRecord>,
    failures: u32,
    locked_until_ms: u64,
    /// (task, expires_ms). `None` vouches for whichever task is answered next.
    grants: Vec<(Option<u64>, u64)>,
}

pub struct PinVault {
    path: PathBuf,
    state: Mutex<State>,
    audit: Option<Arc<AuditLog>>,
}

/// A PIN is 4 to 12 digits.
pub fn valid_pin(pin: &str) -> bool {
    (4..=12).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn derive(salt: &str, rounds: u32, pin: &str) -> String {
    let mut h: [u8; 32] = Sha256::new()
        .chain_update(salt.as_bytes())
        .chain_update(pin.as_bytes())
        .finalize()
        .into();
    for _ in 1..rounds {
        h = Sha256::new()
            .chain_update(h)
            .chain_update(salt.as_bytes())
            .finalize()
            .into();
    }
    hex(&h)
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn random_salt() -> std::io::Result<String> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(hex(&b))
}

impl PinVault {
    /// Load the vault at `path` (a missing file means no PIN is set).
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let record = match std::fs::read(&path) {
            Ok(b) => Some(serde_json::from_slice(&b).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} 不是有效的 PIN 文件：{e}", path.display()),
                )
            })?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        Ok(Self {
            path,
            state: Mutex::new(State {
                record,
                ..State::default()
            }),
            audit: None,
        })
    }

    /// Record every PIN check (never the PIN) in the audit log.
    pub fn with_audit(mut self, audit: Arc<AuditLog>) -> Self {
        self.audit = Some(audit);
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn is_set(&self) -> bool {
        self.lock().record.is_some()
    }

    /// Set (or replace) the PIN.
    pub fn set(&self, pin: &str) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if !valid_pin(pin) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "PIN 必须是 4 到 12 位数字",
            ));
        }
        let salt = random_salt()?;
        let record = PinRecord {
            hash: derive(&salt, ROUNDS, pin),
            salt,
            rounds: ROUNDS,
        };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&serde_json::to_vec(&record).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        let mut s = self.lock();
        s.record = Some(record);
        s.failures = 0;
        s.grants.clear();
        Ok(())
    }

    /// Check `pin`; on success, vouch for the next approval of `task` (or of
    /// any task, for `None`) within a minute.
    pub fn verify_for(&self, task: Option<u64>, pin: &str) -> PinCheck {
        let now = now_ms();
        let result = {
            let mut s = self.lock();
            let s = &mut *s;
            match &s.record {
                None => PinCheck::NotSet,
                Some(_) if now < s.locked_until_ms => PinCheck::Locked {
                    seconds: (s.locked_until_ms - now).div_ceil(1000),
                },
                Some(r) => {
                    if same(&derive(&r.salt, r.rounds, pin), &r.hash) {
                        s.failures = 0;
                        s.grants.retain(|(_, exp)| *exp > now);
                        s.grants.push((task, now + GRANT_MS));
                        PinCheck::Ok
                    } else {
                        s.failures += 1;
                        if s.failures >= MAX_FAILURES {
                            s.failures = 0;
                            s.locked_until_ms = now + LOCKOUT_MS;
                            PinCheck::Locked {
                                seconds: LOCKOUT_MS / 1000,
                            }
                        } else {
                            PinCheck::Wrong {
                                attempts_left: MAX_FAILURES - s.failures,
                            }
                        }
                    }
                }
            }
        };
        if let Some(a) = &self.audit {
            let _ = a.append(AuditEvent::IdentityCheck {
                method: "pin".into(),
                task,
                ok: result == PinCheck::Ok,
                detail: match &result {
                    PinCheck::Ok => "ok".into(),
                    PinCheck::Wrong { .. } => "wrong".into(),
                    PinCheck::Locked { .. } => "locked".into(),
                    PinCheck::NotSet => "not_set".into(),
                },
            });
        }
        result
    }

    /// Use up a grant for `task`, if a recent PIN entry vouched for it.
    pub fn take_grant(&self, task: u64) -> bool {
        let now = now_ms();
        let mut s = self.lock();
        s.grants.retain(|(_, exp)| *exp > now);
        let pos = s
            .grants
            .iter()
            .position(|(t, _)| *t == Some(task))
            .or_else(|| s.grants.iter().position(|(t, _)| t.is_none()));
        match pos {
            Some(i) => {
                s.grants.remove(i);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentos-pin-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d.join("pin.json")
    }

    fn cleanup(p: &Path) {
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn pin_format() {
        assert!(valid_pin("1234"));
        assert!(valid_pin("123456789012"));
        assert!(!valid_pin("123"));
        assert!(!valid_pin("1234567890123"));
        assert!(!valid_pin("12a4"));
        assert!(!valid_pin(" 1234"));
    }

    #[test]
    fn set_verify_and_reload() {
        let p = temp("reload");
        let v = PinVault::open(&p).unwrap();
        assert!(!v.is_set());
        assert_eq!(v.verify_for(Some(1), "1234"), PinCheck::NotSet);
        v.set("2468").unwrap();
        let file = std::fs::read_to_string(&p).unwrap();
        assert!(!file.contains("2468"), "PIN must not be stored in clear");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let v = PinVault::open(&p).unwrap();
        assert!(v.is_set());
        assert_eq!(
            v.verify_for(Some(1), "1111"),
            PinCheck::Wrong { attempts_left: 4 }
        );
        assert_eq!(v.verify_for(Some(1), "2468"), PinCheck::Ok);
        cleanup(&p);
    }

    #[test]
    fn grants_are_per_task_and_single_use() {
        let p = temp("grants");
        let v = PinVault::open(&p).unwrap();
        v.set("2468").unwrap();
        assert!(!v.take_grant(3), "no PIN entered yet");
        assert_eq!(v.verify_for(Some(3), "2468"), PinCheck::Ok);
        assert!(!v.take_grant(4), "a grant for task 3 does not cover task 4");
        assert!(v.take_grant(3));
        assert!(!v.take_grant(3), "grants are used up");
        // Console entry: vouches for whichever task is answered next.
        assert_eq!(v.verify_for(None, "2468"), PinCheck::Ok);
        assert!(v.take_grant(9));
        assert!(!v.take_grant(9));
        cleanup(&p);
    }

    #[test]
    fn too_many_failures_lock_entry() {
        let p = temp("lock");
        let v = PinVault::open(&p).unwrap();
        v.set("2468").unwrap();
        for left in (1..MAX_FAILURES).rev() {
            assert_eq!(
                v.verify_for(None, "0000"),
                PinCheck::Wrong {
                    attempts_left: left
                }
            );
        }
        assert!(matches!(
            v.verify_for(None, "0000"),
            PinCheck::Locked { .. }
        ));
        // Even the right PIN is refused while locked, and grants nothing.
        assert!(matches!(
            v.verify_for(None, "2468"),
            PinCheck::Locked { .. }
        ));
        assert!(!v.take_grant(1));
        cleanup(&p);
    }

    #[test]
    fn checks_are_audited_without_the_pin() {
        let p = temp("audit");
        let audit_path = p.with_file_name("audit.jsonl");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let audit = Arc::new(AuditLog::open(&audit_path).unwrap());
        let v = PinVault::open(&p).unwrap().with_audit(audit);
        v.set("2468").unwrap();
        v.verify_for(Some(2), "1357");
        v.verify_for(Some(2), "2468");
        let log = std::fs::read_to_string(&audit_path).unwrap();
        assert!(!log.contains("2468") && !log.contains("1357"));
        let entries = AuditLog::replay(&audit_path).unwrap();
        let oks: Vec<bool> = entries
            .iter()
            .filter_map(|e| match &e.event {
                AuditEvent::IdentityCheck { ok, .. } => Some(*ok),
                _ => None,
            })
            .collect();
        assert_eq!(oks, vec![false, true]);
        cleanup(&p);
    }
}
