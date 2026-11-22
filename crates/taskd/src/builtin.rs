//! Built-in capabilities: a clock and a notebook with one recoverable clear.
//!
//! The daemon uses an atomic on-disk notebook; `register_all` keeps an in-memory
//! variant for embedders and tests. Both expose exactly the same capabilities.
//!
//! Each note remembers whether a tainted task wrote it, so content that came
//! from outside the user's control does not become trusted once it is stored:
//! `notes.list` taints whoever reads such a note.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentos_core::{
    Capability, CapabilityError, CapabilityName, CapabilitySpec, Intent, Invocation, RiskLevel,
    TAINTED_OUTPUT,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use agentos_services::registry::CapabilityRegistry;

/// The longest note the notebook accepts.
pub const MAX_NOTE_CHARS: usize = 16_384;

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

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        validate_args(&args, &[])?;
        Ok(clock_reading(&jiff::Zoned::now()))
    }
}

/// What `clock.now` says for a moment in some time zone. Someone asking the
/// time means the time where they are, so the reading is local and carries its
/// offset. `unix_ms` stays for clients that render the time themselves.
fn clock_reading(t: &jiff::Zoned) -> Value {
    const WEEKDAYS: [&str; 7] = ["一", "二", "三", "四", "五", "六", "日"];
    let secs = t.offset().seconds();
    let utc_offset = format!(
        "UTC{}{:02}:{:02}",
        if secs < 0 { '-' } else { '+' },
        secs.abs() / 3600,
        secs.abs() / 60 % 60
    );
    let weekday = WEEKDAYS[t.weekday().to_monday_zero_offset() as usize];
    json!({
        "unix_ms": t.timestamp().as_millisecond(),
        "local": t.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string(),
        "utc_offset": utc_offset,
        "message": format!(
            "现在是 {} 星期{weekday} {}（{utc_offset}）",
            t.strftime("%Y-%m-%d"),
            t.strftime("%H:%M:%S")
        ),
    })
}

/// What an agent is told about where and when it runs. The service states
/// these facts; they are not the user's words and not outside content, so
/// they go into the system prompt and never into the intent.
pub fn environment(now: &jiff::Zoned, intent: &Intent) -> String {
    let reading = clock_reading(now);
    format!(
        "当前环境（由系统提供，任务开始或继续执行时更新）：{}；设备 {}（{} {}）；这个意图来自 {}。",
        reading["message"].as_str().unwrap_or_default(),
        hostname().unwrap_or_else(|| "未知".into()),
        std::env::consts::OS,
        std::env::consts::ARCH,
        intent.source,
    )
}

/// This machine's name, if it has a printable one.
fn hostname() -> Option<String> {
    use std::os::raw::{c_char, c_int};

    extern "C" {
        fn gethostname(name: *mut c_char, len: usize) -> c_int;
    }

    let mut buffer = [0u8; 256];
    // SAFETY: `buffer` is writable for the length passed, which leaves its
    // last byte untouched, so the name is NUL-terminated even if truncated.
    if unsafe { gethostname(buffer.as_mut_ptr().cast(), buffer.len() - 1) } != 0 {
        return None;
    }
    let end = buffer.iter().position(|byte| *byte == 0)?;
    let name = std::str::from_utf8(&buffer[..end]).ok()?.trim();
    (!name.is_empty() && name.chars().all(|c| !c.is_control())).then(|| name.to_string())
}

/// The notes' texts, kept current for embedders and tests to look at.
pub type NoteStore = Arc<Mutex<Vec<String>>>;

/// One note and its provenance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "StoredNote", into = "StoredNote")]
struct Note {
    text: String,
    /// Written by a task that had seen content from outside the user's
    /// control.
    tainted: bool,
}

/// A note on disk. A trusted note stays a bare string, so a notebook with
/// no tainted notes is saved exactly as before provenance existed, and old
/// notebooks load with every note trusted.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum StoredNote {
    Plain(String),
    Marked(MarkedNote),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkedNote {
    text: String,
    tainted: bool,
}

impl From<StoredNote> for Note {
    fn from(stored: StoredNote) -> Self {
        match stored {
            StoredNote::Plain(text) => Self {
                text,
                tainted: false,
            },
            StoredNote::Marked(MarkedNote { text, tainted }) => Self { text, tainted },
        }
    }
}

impl From<Note> for StoredNote {
    fn from(note: Note) -> Self {
        if note.tainted {
            Self::Marked(MarkedNote {
                text: note.text,
                tainted: true,
            })
        } else {
            Self::Plain(note.text)
        }
    }
}

impl Note {
    fn new(text: String, call: Invocation) -> Self {
        Self {
            text,
            tainted: call.tainted,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotebookData {
    version: u32,
    notes: Vec<Note>,
    undo: Option<Vec<Note>>,
}

struct Notebook {
    data: Mutex<(Vec<Note>, Option<Vec<Note>>)>,
    texts: NoteStore,
    path: Option<PathBuf>,
}

impl Notebook {
    fn memory() -> Self {
        Self {
            data: Mutex::default(),
            texts: Arc::default(),
            path: None,
        }
    }

    fn notes(&self) -> Vec<Note> {
        self.data
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .0
            .clone()
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
                        .any(|note| {
                            note.text.trim().is_empty()
                                || note.text.chars().count() > MAX_NOTE_CHARS
                        })
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
        let texts = data.notes.iter().map(|note| note.text.clone()).collect();
        Ok(Self {
            data: Mutex::new((data.notes, data.undo)),
            texts: Arc::new(Mutex::new(texts)),
            path: Some(path.to_path_buf()),
        })
    }

    /// Serialize writers across snapshot persistence and in-memory commit.
    fn change<T>(
        &self,
        update: impl FnOnce(&mut NotebookData) -> Result<T, CapabilityError>,
    ) -> Result<T, CapabilityError> {
        let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
        let mut next = NotebookData {
            version: 1,
            notes: data.0.clone(),
            undo: data.1.clone(),
        };
        let result = update(&mut next)?;
        if let Some(path) = &self.path {
            atomic_save(path, &next)
                .map_err(|e| CapabilityError::Failed(format!("保存笔记失败：{e}")))?;
        }
        *self.texts.lock().unwrap_or_else(|p| p.into_inner()) =
            next.notes.iter().map(|note| note.text.clone()).collect();
        *data = (next.notes, next.undo);
        Ok(result)
    }
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn atomic_save(path: &Path, data: &NotebookData) -> io::Result<()> {
    let bytes = serde_json::to_vec(data).map_err(io::Error::other)?;
    agentos_core::durable::write_atomic(path, &bytes)
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

/// Rewrite or remove one note in place. Without these a planner asked to
/// change one note clears the notebook and appends everything again, which
/// spends the single restore snapshot on an edit: the user can then no
/// longer clear, and a restore brings back stale copies.
pub struct NotesUpdate {
    spec: CapabilitySpec,
    notebook: Arc<Notebook>,
}

pub struct NotesRemove {
    spec: CapabilitySpec,
    notebook: Arc<Notebook>,
}

fn note_text(args: &Value, key: &str) -> Result<String, CapabilityError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty() && t.chars().count() <= MAX_NOTE_CHARS)
        .map(str::to_string)
        .ok_or_else(|| {
            CapabilityError::InvalidArgs(format!(
                "`{key}` 必须是不超过 {MAX_NOTE_CHARS} 个字符的非空文字"
            ))
        })
}

/// The 0-based position of note `index` (1-based, as `notes.list` numbers
/// them), but only while it still reads `current`. Other tasks may change
/// the notebook between the list a planner read and this call; matching
/// the text keeps a shifted index from changing the wrong note.
fn locate_note(notes: &[Note], args: &Value) -> Result<usize, CapabilityError> {
    let index = args
        .get("index")
        .and_then(Value::as_u64)
        .filter(|i| *i >= 1)
        .ok_or_else(|| CapabilityError::InvalidArgs("`index` 必须是从 1 开始的笔记编号".into()))?;
    let current = note_text(args, "current")?;
    let position = usize::try_from(index - 1).unwrap_or(usize::MAX);
    match notes.get(position) {
        Some(note) if note.text == current => Ok(position),
        Some(note) => Err(CapabilityError::Failed(format!(
            "第 {index} 条笔记现在是“{}”，不是“{current}”；笔记可能已被改动，请重新列出后再试。没有做任何修改。",
            note.text
        ))),
        None => Err(CapabilityError::Failed(format!(
            "没有第 {index} 条笔记（现在共 {} 条）；没有做任何修改。",
            notes.len()
        ))),
    }
}

#[async_trait]
impl Capability for NotesUpdate {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        self.invoke_with(args, Invocation::default()).await
    }

    // The new text replaces the old one entirely, so its provenance does
    // too: rewriting a tainted note in an untainted task vouches for it.
    // Such a task can only name the note's current text if the user told it
    // (listing the note would have tainted it), so this is the user's call.
    async fn invoke_with(&self, args: Value, call: Invocation) -> Result<Value, CapabilityError> {
        validate_args(&args, &["index", "current", "text"])?;
        let text = note_text(&args, "text")?;
        self.notebook.change(|data| {
            let position = locate_note(&data.notes, &args)?;
            let previous =
                std::mem::replace(&mut data.notes[position], Note::new(text.clone(), call)).text;
            Ok(json!({
                "index": position + 1,
                "previous": previous,
                "message": format!("已把第 {} 条笔记从“{previous}”改为“{text}”", position + 1),
            }))
        })
    }
}

#[async_trait]
impl Capability for NotesRemove {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        validate_args(&args, &["index", "current"])?;
        self.notebook.change(|data| {
            let position = locate_note(&data.notes, &args)?;
            let removed = data.notes.remove(position).text;
            Ok(json!({
                "removed": removed,
                "count": data.notes.len(),
                "message": format!("已删除第 {} 条笔记：{removed}；需要时可以再记一次", position + 1),
            }))
        })
    }
}

#[async_trait]
impl Capability for NotesAppend {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        self.invoke_with(args, Invocation::default()).await
    }

    async fn invoke_with(&self, args: Value, call: Invocation) -> Result<Value, CapabilityError> {
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
            data.notes.push(Note::new(text.to_string(), call));
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
        let notes = self.notebook.notes();
        let message = if notes.is_empty() {
            "笔记是空的".to_string()
        } else {
            notes
                .iter()
                .enumerate()
                .map(|(i, n)| format!("{}. {}", i + 1, n.text))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let texts: Vec<&str> = notes.iter().map(|note| note.text.as_str()).collect();
        let mut out = json!({"notes": texts, "message": message});
        if notes.iter().any(|note| note.tainted) {
            // The butler taints the reading task on this marker.
            out[TAINTED_OUTPUT] = json!(true);
        }
        Ok(out)
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
            let message = if removed == 0 {
                "笔记本来就是空的，没有清空任何内容".to_string()
            } else {
                format!("已清空 {removed} 条笔记；可用“恢复笔记”撤销最近一次清空")
            };
            Ok(json!({"removed": removed, "message": message}))
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
            "清空全部笔记，保留一次可恢复快照；已有快照时须先恢复。只在用户要清空全部笔记时使用，改一条或删一条用 notes.update、notes.remove",
            RiskLevel::Reversible,
            empty_schema(),
        ),
        notebook: notebook.clone(),
    }));
    let edit_schema = |with_text: bool| {
        let mut properties = json!({
            "index": {"type": "integer", "minimum": 1, "description": "notes.list 里的编号，从 1 开始"},
            "current": {"type": "string", "minLength": 1, "maxLength": MAX_NOTE_CHARS, "description": "这条笔记现在的原文，用来确认没有改错"},
        });
        let mut required = vec!["index", "current"];
        if with_text {
            properties["text"] = json!({"type": "string", "minLength": 1, "maxLength": MAX_NOTE_CHARS, "description": "改成的新内容"});
            required.push("text");
        }
        json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
    };
    reg.register(Arc::new(NotesUpdate {
        spec: spec(
            "notes.update",
            "修改一条笔记的内容（先用 notes.list 看编号和原文）；不影响其他笔记和清空快照",
            RiskLevel::Reversible,
            edit_schema(true),
        ),
        notebook: notebook.clone(),
    }));
    reg.register(Arc::new(NotesRemove {
        spec: spec(
            "notes.remove",
            "删除一条笔记（先用 notes.list 看编号和原文）；结果里带着删掉的原文，不影响清空快照",
            RiskLevel::Reversible,
            edit_schema(false),
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
    notebook.texts.clone()
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
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

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

    async fn invoke_as(reg: &CapabilityRegistry, name: &str, args: Value, tainted: bool) -> Value {
        reg.get(&CapabilityName::new(name))
            .unwrap()
            .invoke_with(args, Invocation { tainted })
            .await
            .unwrap()
    }

    // Guards backward compatibility: a notebook saved before provenance
    // existed loads with every note trusted, and a notebook without tainted
    // notes is still saved in the old format.
    #[tokio::test]
    async fn an_old_notebook_loads_as_trusted_and_provenance_survives_restart() {
        let dir = temp_dir("provenance");
        let path = dir.join("notes.json");
        fs::write(&path, r#"{"version":1,"notes":["牛奶"],"undo":["旧的"]}"#).unwrap();
        let mut reg = CapabilityRegistry::new();
        register_persistent(&mut reg, &path).unwrap();
        let listed = invoke(&reg, "notes.list", json!({})).await;
        assert_eq!(listed["notes"], json!(["牛奶"]));
        assert!(!agentos_core::carries_taint(&listed));
        invoke(&reg, "notes.restore", json!({})).await;
        invoke(&reg, "notes.append", json!({"text": "面包"})).await;
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap()["notes"],
            json!(["旧的", "牛奶", "面包"])
        );

        invoke_as(&reg, "notes.append", json!({"text": "外部"}), true).await;
        drop(reg);
        let mut reg = CapabilityRegistry::new();
        let notes = register_persistent(&mut reg, &path).unwrap();
        assert_eq!(*notes.lock().unwrap(), vec!["旧的", "牛奶", "面包", "外部"]);
        let listed = invoke(&reg, "notes.list", json!({})).await;
        assert!(agentos_core::carries_taint(&listed));
        // Clearing and restoring keeps provenance with the note.
        invoke(&reg, "notes.clear", json!({})).await;
        assert!(!agentos_core::carries_taint(
            &invoke(&reg, "notes.list", json!({})).await
        ));
        invoke(&reg, "notes.restore", json!({})).await;
        assert!(agentos_core::carries_taint(
            &invoke(&reg, "notes.list", json!({})).await
        ));
        // Rewriting the note as a whole in an untainted task replaces its
        // provenance; removing it is allowed and takes it along.
        invoke(
            &reg,
            "notes.update",
            json!({"index": 4, "current": "外部", "text": "自己写的"}),
        )
        .await;
        assert!(!agentos_core::carries_taint(
            &invoke(&reg, "notes.list", json!({})).await
        ));
        invoke_as(
            &reg,
            "notes.update",
            json!({"index": 4, "current": "自己写的", "text": "外部"}),
            true,
        )
        .await;
        invoke(&reg, "notes.remove", json!({"index": 4, "current": "外部"})).await;
        assert!(!agentos_core::carries_taint(
            &invoke(&reg, "notes.list", json!({})).await
        ));
        fs::remove_dir_all(dir).unwrap();
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

    #[tokio::test]
    async fn editing_one_note_leaves_the_clear_snapshot_alone() {
        // Guards the restore promise: an edit must not spend the one
        // snapshot that "恢复笔记" brings back.
        let dir = temp_dir("edit");
        let path = dir.join("notes.json");
        let mut reg = CapabilityRegistry::new();
        let notes = register_persistent(&mut reg, &path).unwrap();
        invoke(&reg, "notes.append", json!({"text":"周五交房租"})).await;
        invoke(&reg, "notes.append", json!({"text":"周六去超市"})).await;
        let updated = invoke(
            &reg,
            "notes.update",
            json!({"index": 1, "current": "周五交房租", "text": "周五下午交房租"}),
        )
        .await;
        assert_eq!(updated["previous"], "周五交房租");
        invoke(
            &reg,
            "notes.remove",
            json!({"index": 2, "current": "周六去超市"}),
        )
        .await;
        assert_eq!(*notes.lock().unwrap(), vec!["周五下午交房租"]);
        // No snapshot was taken, so clearing still works and restores exactly.
        invoke(&reg, "notes.clear", json!({})).await;
        invoke(&reg, "notes.restore", json!({})).await;
        assert_eq!(*notes.lock().unwrap(), vec!["周五下午交房租"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_edit_aimed_at_a_note_that_moved_changes_nothing() {
        let mut reg = CapabilityRegistry::new();
        let notes = register_all(&mut reg);
        invoke(&reg, "notes.append", json!({"text":"a"})).await;
        invoke(&reg, "notes.append", json!({"text":"b"})).await;
        for (name, args) in [
            (
                "notes.update",
                json!({"index": 1, "current": "b", "text": "x"}),
            ),
            ("notes.remove", json!({"index": 3, "current": "b"})),
            ("notes.remove", json!({"index": 0, "current": "a"})),
            (
                "notes.update",
                json!({"index": 1, "current": "a", "text": " "}),
            ),
        ] {
            let result = reg
                .get(&CapabilityName::new(name))
                .unwrap()
                .invoke(args.clone())
                .await;
            assert!(result.is_err(), "{name} {args}");
        }
        assert_eq!(*notes.lock().unwrap(), vec!["a", "b"]);
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

    /// A moment at a fixed UTC offset of `hours`.
    fn at(y: i16, mo: i8, d: i8, h: i8, mi: i8, s: i8, hours: i8) -> jiff::Zoned {
        let zone = jiff::tz::TimeZone::fixed(jiff::tz::Offset::constant(hours));
        jiff::civil::date(y, mo, d)
            .at(h, mi, s, 0)
            .to_zoned(zone)
            .unwrap()
    }

    #[test]
    fn clock_reports_local_time_with_offset() {
        let t = at(2026, 11, 1, 22, 30, 5, 8);
        let v = clock_reading(&t);
        assert_eq!(v["utc_offset"], "UTC+08:00");
        assert_eq!(v["local"], "2026-11-01T22:30:05+08:00");
        assert_eq!(
            v["message"],
            "现在是 2026-11-01 星期日 22:30:05（UTC+08:00）"
        );
        // The instant is unchanged; only its presentation is local.
        assert_eq!(v["unix_ms"], t.timestamp().as_millisecond());
        let v = clock_reading(&at(2026, 11, 11, 11, 24, 50, -7));
        assert_eq!(
            v["message"],
            "现在是 2026-11-11 星期三 11:24:50（UTC-07:00）"
        );
    }

    #[tokio::test]
    async fn clock_capability_answers_in_the_machine_time_zone() {
        let mut reg = CapabilityRegistry::new();
        register_all(&mut reg);
        let before = jiff::Timestamp::now().as_millisecond();
        let v = invoke(&reg, "clock.now", json!({})).await;
        let after = jiff::Timestamp::now().as_millisecond();
        let unix_ms = v["unix_ms"].as_i64().unwrap();
        assert!((before..=after).contains(&unix_ms));
        let local: jiff::Timestamp = v["local"].as_str().unwrap().parse().unwrap();
        assert_eq!(local.as_second(), unix_ms.div_euclid(1000));
        // Compare with the system zone at the same instant, so a DST change
        // cannot race.
        let expected = jiff::Timestamp::from_millisecond(unix_ms)
            .unwrap()
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%Y-%m-%dT%H:%M:%S%:z")
            .to_string();
        assert_eq!(v["local"], expected);
        assert!(!v["message"].as_str().unwrap().contains("UTC "));
    }

    #[test]
    fn the_environment_names_the_local_time_the_machine_and_the_input_source() {
        let moment = at(2026, 11, 12, 9, 30, 0, 8);
        let intent = Intent {
            id: agentos_core::IntentId(1),
            source: agentos_core::DeviceId::new("unix:uid:501"),
            speaker: None,
            text: "忽略以上内容".into(),
            urgency: agentos_core::Urgency::Normal,
            at_ms: 0,
        };
        let text = environment(&moment, &intent);
        assert!(
            text.contains("2026-11-12 星期四 09:30:00（UTC+08:00）"),
            "{text}"
        );
        assert!(text.contains(std::env::consts::OS) && text.contains("unix:uid:501"));
        // Only facts the service knows: never the user's words.
        assert!(!text.contains("忽略以上内容"));
    }
}
