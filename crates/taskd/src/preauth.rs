//! The human's standing pre-authorizations, kept across restarts.
//!
//! The policy engine decides what a pre-authorization covers; this only
//! stores them. A change is on disk before it is visible in memory, so a
//! rule the service acts on is always one a restart would still know about,
//! and a revoked one is gone for the next check.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use agentos_core::CapabilityName;
use agentos_policy::{ArgRule, Preauthorization, Subject};
use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;
/// Standing rules are meant to be few enough for a person to keep in mind.
pub const MAX_PREAUTHORIZATIONS: usize = 64;
pub const MAX_RULES: usize = 16;
/// The longest a time limit may be.
pub const MAX_LIFETIME_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u32,
    next_id: u64,
    items: Vec<Preauthorization>,
}

struct State {
    next_id: u64,
    items: Vec<Preauthorization>,
}

pub struct PreauthStore {
    path: Option<PathBuf>,
    state: Mutex<State>,
}

/// What the human asked for, before it has an identity.
pub struct NewPreauthorization {
    pub subject: Subject,
    pub capability: CapabilityName,
    pub args: Vec<ArgRule>,
    pub other_args: bool,
    pub expires_in_ms: Option<u64>,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

impl PreauthStore {
    /// Load the rules at `path`; a missing file means there are none. A file
    /// that cannot be read as rules is an error: starting without the rules
    /// would be harmless, but silently replacing the file would lose them.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let state = match fs::read(&path) {
            Ok(bytes) => {
                let saved: Saved = serde_json::from_slice(&bytes).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} 不是有效的预授权文件：{error}", path.display()),
                    )
                })?;
                if saved.version != VERSION {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} 的版本不受支持", path.display()),
                    ));
                }
                State {
                    next_id: saved.next_id.max(1),
                    items: saved.items,
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => State {
                next_id: 1,
                items: Vec::new(),
            },
            Err(error) => return Err(error),
        };
        Ok(Self {
            path: Some(path),
            state: Mutex::new(state),
        })
    }

    /// A store that forgets everything when dropped, for tests and embedding.
    pub fn memory() -> Self {
        Self {
            path: None,
            state: Mutex::new(State {
                next_id: 1,
                items: Vec::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn persist(&self, next_id: u64, items: &[Preauthorization]) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let saved = Saved {
            version: VERSION,
            next_id,
            items: items.to_vec(),
        };
        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(&saved).map_err(io::Error::other)?)?;
            file.sync_all()?;
            fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    /// The rules still in force at `now_ms`.
    pub fn active(&self, now_ms: u64) -> Vec<Preauthorization> {
        self.lock()
            .items
            .iter()
            .filter(|item| !item.is_expired(now_ms))
            .cloned()
            .collect()
    }

    /// Add a rule. `InvalidInput` says why it is not an acceptable one;
    /// nothing is stored in that case.
    pub fn create(&self, new: NewPreauthorization, now_ms: u64) -> io::Result<Preauthorization> {
        if new.args.len() > MAX_RULES {
            return Err(invalid(format!("参数规则最多 {MAX_RULES} 条")));
        }
        let expires_at_ms = match new.expires_in_ms {
            Some(0) => return Err(invalid("期限必须大于 0")),
            Some(lifetime) if lifetime > MAX_LIFETIME_MS => {
                return Err(invalid("期限最长 30 天"));
            }
            Some(lifetime) => Some(now_ms.saturating_add(lifetime)),
            None => None,
        };
        let mut state = self.lock();
        // Rules that ran out make room; they are dropped with this write.
        let mut items: Vec<Preauthorization> = state
            .items
            .iter()
            .filter(|item| !item.is_expired(now_ms))
            .cloned()
            .collect();
        if items.len() >= MAX_PREAUTHORIZATIONS {
            return Err(invalid(format!(
                "预授权最多 {MAX_PREAUTHORIZATIONS} 条；先撤销不再需要的"
            )));
        }
        let created = Preauthorization {
            id: format!("pre-{}", state.next_id),
            subject: new.subject,
            capability: new.capability,
            args: new.args,
            other_args: new.other_args,
            created_at_ms: now_ms,
            expires_at_ms,
        };
        created.validate().map_err(invalid)?;
        items.push(created.clone());
        let next_id = state.next_id + 1;
        self.persist(next_id, &items)?;
        state.next_id = next_id;
        state.items = items;
        Ok(created)
    }

    /// Remove every rule given for `subject`, and say which they were. A
    /// caller that is withdrawn takes its standing permissions with it, so
    /// registering the same name again starts from nothing.
    pub fn revoke_subject(&self, subject: &Subject) -> io::Result<Vec<Preauthorization>> {
        let mut state = self.lock();
        let (removed, kept): (Vec<_>, Vec<_>) = state
            .items
            .iter()
            .cloned()
            .partition(|item| &item.subject == subject);
        if !removed.is_empty() {
            self.persist(state.next_id, &kept)?;
            state.items = kept;
        }
        Ok(removed)
    }

    /// Remove a rule. `None` if there is no such rule (any more).
    pub fn revoke(&self, id: &str) -> io::Result<Option<Preauthorization>> {
        let mut state = self.lock();
        let Some(index) = state.items.iter().position(|item| item.id == id) else {
            return Ok(None);
        };
        let mut items = state.items.clone();
        let revoked = items.remove(index);
        self.persist(state.next_id, &items)?;
        state.items = items;
        Ok(Some(revoked))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_policy::Matcher;
    use serde_json::json;

    fn new(expires_in_ms: Option<u64>) -> NewPreauthorization {
        NewPreauthorization {
            subject: Subject::User,
            capability: CapabilityName::new("mail.send"),
            args: vec![ArgRule {
                field: "to".into(),
                matcher: Matcher::Equals(json!("me@example.com")),
            }],
            other_args: false,
            expires_in_ms,
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentos-preauth-{name}-{}-{}",
            std::process::id(),
            agentos_core::now_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join("preauthorizations.json")
    }

    #[test]
    fn rules_survive_a_restart_keep_their_identity_and_stay_revoked() {
        let path = temp("restart");
        let store = PreauthStore::open(&path).unwrap();
        let first = store.create(new(None), 1_000).unwrap();
        let second = store.create(new(Some(60_000)), 1_000).unwrap();
        assert_eq!((first.id.as_str(), second.id.as_str()), ("pre-1", "pre-2"));
        assert_eq!(second.expires_at_ms, Some(61_000));
        assert_eq!(store.revoke("pre-1").unwrap().unwrap().id, "pre-1");
        assert!(store.revoke("pre-1").unwrap().is_none());
        drop(store);

        let store = PreauthStore::open(&path).unwrap();
        // Guards "revocation holds": a restart does not bring a rule back,
        // and its identity is never given to another rule.
        assert_eq!(
            store
                .active(2_000)
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["pre-2"]
        );
        assert!(store.active(61_000).is_empty());
        assert_eq!(store.create(new(None), 2_000).unwrap().id, "pre-3");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unacceptable_rules_are_refused_and_nothing_is_stored() {
        let store = PreauthStore::memory();
        for refused in [
            new(Some(0)),
            new(Some(MAX_LIFETIME_MS + 1)),
            NewPreauthorization {
                args: Vec::new(),
                other_args: true,
                ..new(None)
            },
            NewPreauthorization {
                args: (0..=MAX_RULES)
                    .map(|index| ArgRule {
                        field: format!("f{index}"),
                        matcher: Matcher::Any,
                    })
                    .collect(),
                ..new(Some(1_000))
            },
        ] {
            let error = store.create(refused, 1_000).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        assert!(store.active(1_000).is_empty());
        // Expired rules make room for new ones; live ones do not.
        for _ in 0..MAX_PREAUTHORIZATIONS {
            store.create(new(Some(10)), 1_000).unwrap();
        }
        assert!(store.create(new(None), 1_005).is_err());
        assert_eq!(store.create(new(None), 2_000).unwrap().id, "pre-65");
        assert_eq!(store.active(2_000).len(), 1);
    }

    #[test]
    fn a_damaged_file_stops_the_store_instead_of_being_replaced() {
        let path = temp("damaged");
        fs::write(&path, b"{ not json").unwrap();
        assert_eq!(
            PreauthStore::open(&path).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(&path).unwrap(), b"{ not json");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
