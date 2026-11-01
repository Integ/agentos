//! Built-in capabilities for P0: a clock and a notebook.
//! The notebook spans all three risk tiers so the policy flow can be exercised.
//! With [`register_with_notes_file`] the notes survive a daemon restart.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentos_core::{Capability, CapabilityError, CapabilityName, CapabilitySpec, RiskLevel};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::registry::CapabilityRegistry;

fn spec(name: &str, description: &str, risk: RiskLevel, input_schema: Value) -> CapabilitySpec {
    CapabilitySpec {
        name: CapabilityName::new(name),
        description: description.into(),
        risk,
        input_schema,
    }
}

fn empty_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

pub struct ClockNow {
    spec: CapabilitySpec,
}

impl Default for ClockNow {
    fn default() -> Self {
        Self {
            spec: spec(
                "clock.now",
                "当前日期和时间（本机时区）",
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

    async fn invoke(&self, _args: Value) -> Result<Value, CapabilityError> {
        Ok(clock_reading(chrono::Local::now()))
    }
}

/// What `clock.now` says for a moment in some time zone.
fn clock_reading<Tz: chrono::TimeZone>(t: chrono::DateTime<Tz>) -> Value
where
    Tz::Offset: std::fmt::Display,
{
    use chrono::{Datelike, Offset};
    const WEEKDAYS: [&str; 7] = ["一", "二", "三", "四", "五", "六", "日"];
    let secs = t.offset().fix().local_minus_utc();
    let utc_offset = format!(
        "UTC{}{:02}:{:02}",
        if secs < 0 { '-' } else { '+' },
        secs.abs() / 3600,
        secs.abs() / 60 % 60
    );
    let weekday = WEEKDAYS[t.weekday().num_days_from_monday() as usize];
    json!({
        "unix_ms": t.timestamp_millis(),
        "local": t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        "utc_offset": utc_offset,
        "message": format!(
            "现在是 {} 星期{weekday} {}（{utc_offset}）",
            t.format("%Y-%m-%d"),
            t.format("%H:%M:%S")
        ),
    })
}

pub type NoteStore = Arc<Mutex<Vec<String>>>;

fn lock(store: &NoteStore) -> std::sync::MutexGuard<'_, Vec<String>> {
    store.lock().unwrap_or_else(|p| p.into_inner())
}

/// Where the notebook is saved, if anywhere.
type NotesFile = Option<Arc<PathBuf>>;

/// Load notes saved by [`save_notes`]. A missing file is an empty notebook.
fn load_notes(path: &Path) -> std::io::Result<Vec<String>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} 不是有效的笔记文件：{e}", path.display()),
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Write the notebook atomically (temp file + fsync + rename), so a power cut
/// leaves either the old or the new notes, never half a file.
fn save_notes(path: &Path, notes: &[String]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(notes).map_err(std::io::Error::other)?)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Apply `change` to the notes and persist them. If saving fails the change
/// is undone, so memory and disk never disagree.
fn mutate<T>(
    store: &NoteStore,
    file: &NotesFile,
    change: impl FnOnce(&mut Vec<String>) -> T,
) -> Result<(T, usize), CapabilityError> {
    let mut notes = lock(store);
    let before = file.as_ref().map(|_| notes.clone());
    let out = change(&mut notes);
    if let (Some(path), Some(before)) = (file, before) {
        if let Err(e) = save_notes(path, &notes) {
            *notes = before;
            return Err(CapabilityError::Failed(format!("笔记保存失败：{e}")));
        }
    }
    Ok((out, notes.len()))
}

pub struct NotesAppend {
    spec: CapabilitySpec,
    store: NoteStore,
    file: NotesFile,
}

pub struct NotesList {
    spec: CapabilitySpec,
    store: NoteStore,
}

pub struct NotesClear {
    spec: CapabilitySpec,
    store: NoteStore,
    file: NotesFile,
}

#[async_trait]
impl Capability for NotesAppend {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                CapabilityError::InvalidArgs("`text` must be a non-empty string".into())
            })?;
        let ((), count) = mutate(&self.store, &self.file, |n| n.push(text.to_string()))?;
        Ok(json!({"count": count, "message": format!("已记下：{text}")}))
    }
}

#[async_trait]
impl Capability for NotesList {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, _args: Value) -> Result<Value, CapabilityError> {
        let notes = lock(&self.store);
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

    async fn invoke(&self, _args: Value) -> Result<Value, CapabilityError> {
        let (n, _) = mutate(&self.store, &self.file, |notes| {
            let n = notes.len();
            notes.clear();
            n
        })?;
        Ok(json!({"removed": n, "message": format!("已删除 {n} 条笔记")}))
    }
}

/// Register every built-in capability with an in-memory notebook (lost when
/// the daemon exits). Returns the shared note store.
pub fn register_all(reg: &mut CapabilityRegistry) -> NoteStore {
    register(reg, Arc::default(), None)
}

/// Register every built-in capability with the notebook kept in `notes_file`
/// (JSON array of strings), loading what is already there.
pub fn register_with_notes_file(
    reg: &mut CapabilityRegistry,
    notes_file: impl Into<PathBuf>,
) -> std::io::Result<NoteStore> {
    let path = notes_file.into();
    let store = Arc::new(Mutex::new(load_notes(&path)?));
    Ok(register(reg, store, Some(Arc::new(path))))
}

fn register(reg: &mut CapabilityRegistry, store: NoteStore, file: NotesFile) -> NoteStore {
    reg.register(Arc::new(ClockNow::default()));
    reg.register(Arc::new(NotesAppend {
        spec: spec(
            "notes.append",
            "追加一条笔记",
            RiskLevel::Reversible,
            json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
        ),
        store: store.clone(),
        file: file.clone(),
    }));
    reg.register(Arc::new(NotesList {
        spec: spec(
            "notes.list",
            "列出所有笔记",
            RiskLevel::ReadOnly,
            empty_schema(),
        ),
        store: store.clone(),
    }));
    reg.register(Arc::new(NotesClear {
        spec: spec(
            "notes.clear",
            "删除全部笔记（不可恢复）",
            RiskLevel::Irreversible,
            empty_schema(),
        ),
        store: store.clone(),
        file,
    }));
    store
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::CapabilityName;
    use chrono::TimeZone;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentos-notes-{name}-{}-{}",
            std::process::id(),
            agentos_core::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("sub/notes.json")
    }

    async fn call(reg: &CapabilityRegistry, name: &str, args: Value) -> Value {
        reg.get(&CapabilityName::new(name))
            .expect("registered")
            .invoke(args)
            .await
            .expect("ok")
    }

    #[test]
    fn clock_reports_local_time_with_offset() {
        let tz = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let t = tz.with_ymd_and_hms(2026, 11, 1, 22, 30, 5).unwrap();
        let v = clock_reading(t);
        assert_eq!(v["utc_offset"], "UTC+08:00");
        assert_eq!(v["local"], "2026-11-01T22:30:05+08:00");
        assert_eq!(
            v["message"],
            "现在是 2026-11-01 星期日 22:30:05（UTC+08:00）"
        );
        let west = chrono::FixedOffset::west_opt(3 * 3600 + 30 * 60).unwrap();
        let v = clock_reading(west.with_ymd_and_hms(2026, 1, 5, 8, 0, 0).unwrap());
        assert_eq!(v["utc_offset"], "UTC-03:30");
    }

    #[tokio::test]
    async fn notes_survive_a_restart() {
        let path = temp_file("restart");
        let mut reg = CapabilityRegistry::new();
        register_with_notes_file(&mut reg, &path).unwrap();
        call(&reg, "notes.append", json!({"text": "买牛奶"})).await;
        call(&reg, "notes.append", json!({"text": "交电费"})).await;

        // A new daemon sees the same notes.
        let mut reg = CapabilityRegistry::new();
        let store = register_with_notes_file(&mut reg, &path).unwrap();
        assert_eq!(*lock(&store), vec!["买牛奶", "交电费"]);
        let v = call(&reg, "notes.clear", json!({})).await;
        assert_eq!(v["removed"], 2);

        let mut reg = CapabilityRegistry::new();
        let store = register_with_notes_file(&mut reg, &path).unwrap();
        assert!(lock(&store).is_empty());
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn corrupt_notes_file_is_an_error_not_silently_emptied() {
        let path = temp_file("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        let mut reg = CapabilityRegistry::new();
        let e = register_with_notes_file(&mut reg, &path).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn failed_save_leaves_notes_unchanged() {
        // The "file" is a directory, so every save fails.
        let path = temp_file("unwritable");
        std::fs::create_dir_all(&path).unwrap();
        let store: NoteStore = Arc::default();
        let mut reg = CapabilityRegistry::new();
        register(&mut reg, store.clone(), Some(Arc::new(path.clone())));
        let r = reg
            .get(&CapabilityName::new("notes.append"))
            .unwrap()
            .invoke(json!({"text": "x"}))
            .await;
        assert!(matches!(r, Err(CapabilityError::Failed(_))));
        assert!(lock(&store).is_empty());
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }
}
