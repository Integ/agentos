//! Built-in capabilities: a clock and a notebook with one recoverable clear.
//!
//! The daemon uses an atomic on-disk notebook; `register_all` keeps an in-memory
//! variant for embedders and tests. Both expose exactly the same capabilities.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agentos_core::{
    now_ms, Capability, CapabilityError, CapabilityName, CapabilitySpec, RiskLevel,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::registry::CapabilityRegistry;

const MAX_NOTE_CHARS: usize = 16_384;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn spec(name: &str, description: &str, risk: RiskLevel, input_schema: Value) -> CapabilitySpec {
    CapabilitySpec {
        name: CapabilityName::new(name),
        description: description.into(),
        risk,
        input_schema,
    }
}

fn empty_schema() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": false})
}

fn validate_args(args: &Value, allowed: &[&str]) -> Result<(), CapabilityError> {
    let object = args
        .as_object()
        .ok_or_else(|| CapabilityError::InvalidArgs("arguments must be an object".into()))?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(CapabilityError::InvalidArgs(
            "unexpected argument property".into(),
        ));
    }
    Ok(())
}

pub struct ClockNow {
    spec: CapabilitySpec,
}

impl Default for ClockNow {
    fn default() -> Self {
        Self {
            spec: spec(
                "clock.now",
                "当前时间（UTC）",
                RiskLevel::ReadOnly,
                empty_schema(),
            ),
        }
    }
}

#[async_trait]
impl Capability for ClockNow {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        validate_args(&args, &[])?;
        let ms = now_ms();
        let secs = ms / 1000 % 86_400;
        let hms = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
        Ok(json!({"unix_ms": ms, "message": format!("现在是 UTC {hms}")}))
    }
}

pub type NoteStore = Arc<Mutex<Vec<String>>>;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotebookData {
    version: u32,
    notes: Vec<String>,
    undo: Option<Vec<String>>,
}

struct Notebook {
    notes: NoteStore,
    undo: Mutex<Option<Vec<String>>>,
    path: Option<PathBuf>,
}

impl Notebook {
    fn memory() -> Self {
        Self {
            notes: Arc::default(),
            undo: Mutex::new(None),
            path: None,
        }
    }

    fn open(path: &Path) -> io::Result<Self> {
        let parent = parent_dir(path);
        fs::create_dir_all(parent)?;
        let data = match fs::read(path) {
            Ok(bytes) => {
                let data: NotebookData = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                if data.version != 1
                    || data
                        .notes
                        .iter()
                        .chain(data.undo.iter().flatten())
                        .any(|note| note.trim().is_empty() || note.chars().count() > MAX_NOTE_CHARS)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unsupported or invalid notebook data",
                    ));
                }
                data
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => NotebookData {
                version: 1,
                notes: Vec::new(),
                undo: None,
            },
            Err(e) => return Err(e),
        };
        // Validate persistence before exposing write capabilities.
        if !path.exists() {
            atomic_save(path, &data)?;
        }
        Ok(Self {
            notes: Arc::new(Mutex::new(data.notes)),
            undo: Mutex::new(data.undo),
            path: Some(path.to_path_buf()),
        })
    }

    /// Serialize writers across snapshot persistence and in-memory commit.
    fn change<T>(
        &self,
        update: impl FnOnce(&mut NotebookData) -> Result<T, CapabilityError>,
    ) -> Result<T, CapabilityError> {
        let mut notes = self.notes.lock().unwrap_or_else(|p| p.into_inner());
        let mut undo = self.undo.lock().unwrap_or_else(|p| p.into_inner());
        let mut next = NotebookData {
            version: 1,
            notes: notes.clone(),
            undo: undo.clone(),
        };
        let result = update(&mut next)?;
        if let Some(path) = &self.path {
            atomic_save(path, &next)
                .map_err(|e| CapabilityError::Failed(format!("保存笔记失败：{e}")))?;
        }
        *notes = next.notes;
        *undo = next.undo;
        Ok(result)
    }
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn atomic_save(path: &Path, data: &NotebookData) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "notebook needs a file name"))?;
    let mut temp_name = name.to_os_string();
    temp_name.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let temp = parent_dir(path).join(temp_name);
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp)?;
        let bytes = serde_json::to_vec(data).map_err(io::Error::other)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent_dir(path))?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

pub struct NotesAppend {
    spec: CapabilitySpec,
    notebook: Arc<Notebook>,
}

pub struct NotesList {
    spec: CapabilitySpec,
    notebook: Arc<Notebook>,
}

pub struct NotesClear {
    spec: CapabilitySpec,
    notebook: Arc<Notebook>,
}

pub struct NotesRestore {
    spec: CapabilitySpec,
    notebook: Arc<Notebook>,
}

#[async_trait]
impl Capability for NotesAppend {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        validate_args(&args, &["text"])?;
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty() && t.chars().count() <= MAX_NOTE_CHARS)
            .ok_or_else(|| {
                CapabilityError::InvalidArgs(format!(
                    "`text` must be a non-empty string of at most {MAX_NOTE_CHARS} characters"
                ))
            })?;
        self.notebook.change(|data| {
            data.notes.push(text.to_string());
            Ok(json!({"count": data.notes.len(), "message": format!("已记下：{text}")}))
        })
    }
}

#[async_trait]
impl Capability for NotesList {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        validate_args(&args, &[])?;
        let notes = self
            .notebook
            .notes
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let message = if notes.is_empty() {
            "笔记是空的".to_string()
        } else {
            notes
                .iter()
                .enumerate()
                .map(|(i, n)| format!("{}. {n}", i + 1))
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(json!({"notes": *notes, "message": message}))
    }
}

#[async_trait]
impl Capability for NotesClear {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        validate_args(&args, &[])?;
        self.notebook.change(|data| {
            let removed = data.notes.len();
            if removed != 0 {
                if data.undo.is_some() {
                    return Err(CapabilityError::Failed(
                        "仍有上次清空的可恢复快照；请先“恢复笔记”再清空，避免覆盖备份。现有笔记和快照均已保留。".into(),
                    ));
                }
                data.undo = Some(std::mem::take(&mut data.notes));
            }
            Ok(json!({"removed": removed, "message": format!("已清空 {removed} 条笔记；可用“恢复笔记”撤销最近一次清空")}))
        })
    }
}

#[async_trait]
impl Capability for NotesRestore {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        validate_args(&args, &[])?;
        self.notebook.change(|data| {
            let Some(mut restored) = data.undo.take() else {
                return Ok(json!({"restored": 0, "message": "没有可恢复的清空记录"}));
            };
            let count = restored.len();
            restored.append(&mut data.notes);
            data.notes = restored;
            Ok(json!({"restored": count, "count": data.notes.len(), "message": format!("已恢复 {count} 条笔记，保留清空后新增的笔记")}))
        })
    }
}

fn register(reg: &mut CapabilityRegistry, notebook: Notebook) -> NoteStore {
    let notebook = Arc::new(notebook);
    reg.register(Arc::new(ClockNow::default()));
    reg.register(Arc::new(NotesAppend {
        spec: spec(
            "notes.append",
            "追加一条笔记",
            RiskLevel::Reversible,
            json!({"type": "object", "properties": {"text": {"type": "string", "minLength": 1, "maxLength": MAX_NOTE_CHARS}}, "required": ["text"], "additionalProperties": false}),
        ),
        notebook: notebook.clone(),
    }));
    reg.register(Arc::new(NotesList {
        spec: spec(
            "notes.list",
            "列出所有笔记",
            RiskLevel::ReadOnly,
            empty_schema(),
        ),
        notebook: notebook.clone(),
    }));
    reg.register(Arc::new(NotesClear {
        spec: spec(
            "notes.clear",
            "清空全部笔记，保留一次可恢复快照；已有快照时须先恢复",
            RiskLevel::Reversible,
            empty_schema(),
        ),
        notebook: notebook.clone(),
    }));
    reg.register(Arc::new(NotesRestore {
        spec: spec(
            "notes.restore",
            "恢复最近一次清空，并保留清空后新增的笔记",
            RiskLevel::Reversible,
            empty_schema(),
        ),
        notebook: notebook.clone(),
    }));
    notebook.notes.clone()
}

/// Register an in-memory notebook, primarily for tests and embedding.
pub fn register_all(reg: &mut CapabilityRegistry) -> NoteStore {
    register(reg, Notebook::memory())
}

/// Register a durable notebook. Invalid existing data is never overwritten.
pub fn register_persistent(
    reg: &mut CapabilityRegistry,
    path: impl AsRef<Path>,
) -> io::Result<NoteStore> {
    Ok(register(reg, Notebook::open(path.as_ref())?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agentos-notes-{label}-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    async fn invoke(reg: &CapabilityRegistry, name: &str, args: Value) -> Value {
        reg.get(&CapabilityName::new(name))
            .unwrap()
            .invoke(args)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn notes_and_undo_survive_restart_without_losing_new_notes() {
        let dir = temp_dir("restart");
        let path = dir.join("notes.json");
        let mut reg = CapabilityRegistry::new();
        register_persistent(&mut reg, &path).unwrap();
        invoke(&reg, "notes.append", json!({"text":"before"})).await;
        invoke(&reg, "notes.clear", json!({})).await;
        // Clearing an empty notebook does not destroy the undo snapshot.
        invoke(&reg, "notes.clear", json!({})).await;
        invoke(&reg, "notes.append", json!({"text":"after"})).await;
        drop(reg);
        let mut reopened = CapabilityRegistry::new();
        let notes = register_persistent(&mut reopened, &path).unwrap();
        assert_eq!(*notes.lock().unwrap(), vec!["after"]);
        let restored = invoke(&reopened, "notes.restore", json!({})).await;
        assert_eq!(restored["restored"], 1);
        assert_eq!(*notes.lock().unwrap(), vec!["before", "after"]);
        drop(reopened);
        let mut final_reg = CapabilityRegistry::new();
        let final_notes = register_persistent(&mut final_reg, &path).unwrap();
        assert_eq!(*final_notes.lock().unwrap(), vec!["before", "after"]);
        assert_eq!(
            invoke(&final_reg, "notes.restore", json!({})).await["restored"],
            0
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn another_clear_cannot_destroy_an_unrestored_snapshot() {
        let dir = temp_dir("snapshot-protection");
        let path = dir.join("notes.json");
        let mut reg = CapabilityRegistry::new();
        let notes = register_persistent(&mut reg, &path).unwrap();
        invoke(&reg, "notes.append", json!({"text":"before"})).await;
        invoke(&reg, "notes.clear", json!({})).await;
        invoke(&reg, "notes.append", json!({"text":"after"})).await;
        let before = fs::read(&path).unwrap();
        let error = reg
            .get(&CapabilityName::new("notes.clear"))
            .unwrap()
            .invoke(json!({}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("先“恢复笔记”"));
        assert_eq!(*notes.lock().unwrap(), vec!["after"]);
        assert_eq!(fs::read(&path).unwrap(), before);
        drop(reg);
        let mut reopened = CapabilityRegistry::new();
        let notes = register_persistent(&mut reopened, &path).unwrap();
        invoke(&reopened, "notes.restore", json!({})).await;
        assert_eq!(*notes.lock().unwrap(), vec!["before", "after"]);
        // Once restored, the next clear safely snapshots all active notes.
        invoke(&reopened, "notes.clear", json!({})).await;
        assert!(notes.lock().unwrap().is_empty());
        invoke(&reopened, "notes.restore", json!({})).await;
        assert_eq!(*notes.lock().unwrap(), vec!["before", "after"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_notebook_is_preserved() {
        let dir = temp_dir("corrupt");
        let path = dir.join("notes.json");
        let bytes = b"{broken";
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            register_persistent(&mut CapabilityRegistry::new(), &path)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn failed_persistence_does_not_commit_memory_or_undo() {
        let dir = temp_dir("failure");
        let path = dir.join("notes.json");
        let mut reg = CapabilityRegistry::new();
        let notes = register_persistent(&mut reg, &path).unwrap();
        invoke(&reg, "notes.append", json!({"text":"keep"})).await;
        // A directory cannot be atomically replaced by the notebook file.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let clear = reg.get(&CapabilityName::new("notes.clear")).unwrap();
        assert!(clear.invoke(json!({})).await.is_err());
        assert_eq!(*notes.lock().unwrap(), vec!["keep"]);
        fs::remove_dir(&path).unwrap();
        assert_eq!(
            invoke(&reg, "notes.restore", json!({})).await["restored"],
            0
        );
        assert_eq!(*notes.lock().unwrap(), vec!["keep"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn invalid_arguments_never_modify_notebook() {
        let mut reg = CapabilityRegistry::new();
        let notes = register_all(&mut reg);
        for args in [
            json!(null),
            json!({"text":"  "}),
            json!({"text":"ok","extra":true}),
        ] {
            assert!(reg
                .get(&CapabilityName::new("notes.append"))
                .unwrap()
                .invoke(args)
                .await
                .is_err());
        }
        for name in ["notes.list", "notes.clear", "notes.restore", "clock.now"] {
            assert!(reg
                .get(&CapabilityName::new(name))
                .unwrap()
                .invoke(json!({"extra":true}))
                .await
                .is_err());
        }
        assert!(notes.lock().unwrap().is_empty());
    }
}
