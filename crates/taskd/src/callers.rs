//! Outside programs the user lets submit intents, kept across restarts.
//!
//! A caller is a narrowed principal: its tasks may use what its manifest
//! names, what the butler itself holds, and nothing above its risk ceiling.
//! This only stores the manifests. As with pre-authorizations, a change is
//! on disk before it is visible, so a caller the service acts for is always
//! one a restart would still know, and a revoked one stays revoked.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use agentos_core::durable::write_atomic;

use agentos_ipc::Caller;
use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;
pub const MAX_CALLERS: usize = 32;
pub const MAX_PATTERNS: usize = 64;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u32,
    callers: Vec<Caller>,
}

pub struct CallerStore {
    path: Option<PathBuf>,
    callers: Mutex<Vec<Caller>>,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// Why `caller` is not an acceptable manifest, if it is not.
pub fn validate(caller: &Caller) -> Result<(), String> {
    let plain = |text: &str| {
        text.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    };
    if caller.id.is_empty() || caller.id.len() > 32 || !plain(&caller.id) {
        return Err("调用方 id 只能用小写字母、数字、- 和 _，最长 32 个字符".into());
    }
    let name = caller.name.trim();
    if name.is_empty() || name.len() > 120 || name.chars().any(char::is_control) {
        return Err("调用方需要一个不超过 120 字节的名字".into());
    }
    if caller.capabilities.len() > MAX_PATTERNS {
        return Err(format!("能力最多列出 {MAX_PATTERNS} 项"));
    }
    for pattern in &caller.capabilities {
        let name = pattern.strip_suffix('*').unwrap_or(pattern);
        // "Everything" is not something to hand an outside program in one
        // character: a prefix has to name at least a namespace.
        if name.is_empty()
            || pattern.len() > 128
            || name.contains('*')
            || name.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(format!(
                "能力 {pattern:?} 无效：写能力名，或以 * 结尾的前缀（例如 notes.*）"
            ));
        }
    }
    Ok(())
}

impl CallerStore {
    /// Load the manifests at `path`; a missing file means there are none. A
    /// file that cannot be read is an error rather than "no callers", so it
    /// is never silently replaced.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let callers = match fs::read(&path) {
            Ok(bytes) => {
                let saved: Saved = serde_json::from_slice(&bytes).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} 不是有效的调用方文件：{error}", path.display()),
                    )
                })?;
                if saved.version != VERSION {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} 的版本不受支持", path.display()),
                    ));
                }
                saved.callers
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            path: Some(path),
            callers: Mutex::new(callers),
        })
    }

    /// A store that forgets everything when dropped, for tests and embedding.
    pub fn memory() -> Self {
        Self {
            path: None,
            callers: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Caller>> {
        self.callers.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn persist(&self, callers: &[Caller]) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let saved = Saved {
            version: VERSION,
            callers: callers.to_vec(),
        };
        // Durable through the directory entry: a revocation that a power cut
        // could undo would silently restore access.
        write_atomic(
            path,
            &serde_json::to_vec_pretty(&saved).map_err(io::Error::other)?,
        )
    }

    pub fn list(&self) -> Vec<Caller> {
        self.lock().clone()
    }

    pub fn get(&self, id: &str) -> Option<Caller> {
        self.lock().iter().find(|caller| caller.id == id).cloned()
    }

    /// Register `caller`, or replace the manifest of the one with its id.
    /// `InvalidInput` says why it was refused; nothing changes in that case.
    pub fn put(&self, caller: Caller) -> io::Result<Caller> {
        validate(&caller).map_err(invalid)?;
        let mut current = self.lock();
        let mut callers = current.clone();
        match callers.iter().position(|known| known.id == caller.id) {
            Some(index) => callers[index] = caller.clone(),
            None if callers.len() >= MAX_CALLERS => {
                return Err(invalid(format!("调用方最多 {MAX_CALLERS} 个")));
            }
            None => callers.push(caller.clone()),
        }
        self.persist(&callers)?;
        *current = callers;
        Ok(caller)
    }

    /// Remove a caller. `None` if there is no such caller (any more).
    pub fn remove(&self, id: &str) -> io::Result<Option<Caller>> {
        let mut current = self.lock();
        let Some(index) = current.iter().position(|caller| caller.id == id) else {
            return Ok(None);
        };
        let mut callers = current.clone();
        let removed = callers.remove(index);
        self.persist(&callers)?;
        *current = callers;
        Ok(Some(removed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::RiskLevel;

    fn caller(id: &str, capabilities: &[&str]) -> Caller {
        Caller {
            id: id.into(),
            name: "测试助手".into(),
            capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
            max_risk: RiskLevel::ReadOnly,
            created_at_ms: 1,
        }
    }

    #[test]
    fn a_manifest_names_what_it_gives_and_never_everything() {
        assert!(validate(&caller("helper", &[])).is_ok());
        assert!(validate(&caller("helper", &["notes.list", "home.*"])).is_ok());
        for id in ["", "Helper", "a b", "caller:x", &"x".repeat(33)] {
            assert!(validate(&caller(id, &[])).is_err(), "{id}");
        }
        // Guards "a caller holds nothing it was not given": no wildcard for
        // everything, no pattern that is not a name or a prefix.
        for pattern in ["*", "", "no*tes", "notes.* x", "**"] {
            assert!(
                validate(&caller("helper", &[pattern])).is_err(),
                "{pattern}"
            );
        }
        let given = caller("helper", &["notes.list", "home.*"]);
        assert!(given.wants("notes.list") && given.wants("home.state"));
        assert!(!given.wants("notes.append") && !given.wants("homework.do"));
        assert_eq!(given.source(), "caller:helper");
    }

    #[test]
    fn manifests_survive_a_restart_are_replaced_whole_and_stay_revoked() {
        let dir = std::env::temp_dir().join(format!(
            "agentos-callers-{}-{}",
            std::process::id(),
            agentos_core::now_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("callers.json");
        let store = CallerStore::open(&path).unwrap();
        store.put(caller("helper", &["notes.*"])).unwrap();
        store.put(caller("other", &[])).unwrap();
        // Registering again replaces the manifest; nothing is merged in.
        store.put(caller("helper", &["clock.now"])).unwrap();
        assert!(store.put(caller("Bad Id", &[])).is_err());
        assert_eq!(store.remove("other").unwrap().unwrap().id, "other");
        assert!(store.remove("other").unwrap().is_none());
        drop(store);
        let store = CallerStore::open(&path).unwrap();
        assert_eq!(store.list(), vec![caller("helper", &["clock.now"])]);
        assert!(store.get("other").is_none());

        fs::write(&path, b"{ not json").unwrap();
        assert_eq!(
            CallerStore::open(&path).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        let _ = fs::remove_dir_all(dir);
    }
}
