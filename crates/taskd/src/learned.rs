//! What the system has learned from its own history and adopted: lessons
//! (short text the butler's planner reads before every task) and skills
//! (narrow sub-agents for call sequences that keep recurring).
//!
//! Stage 3 of self-improvement (docs/self-improvement-research.md). Nothing
//! here widens what the system may do: a lesson is text, a skill is a
//! delegate whose capabilities are narrowed like any other, and every call
//! either makes still goes through the policy engine. That is why adopting
//! them needs no approval, while each one stays visible and can be undone.
//!
//! Stored in `<state>/improve/learned.json`. Entries are never deleted: a
//! withdrawn one keeps its record and reason. The daemon picks up changes on
//! its own, so a rollback takes effect for the next task.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use agentos_core::RiskLevel;
use agentos_services::agents::AgentManifest;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const FILE: &str = "learned.json";
/// A lesson longer than this is a policy, not a lesson.
pub const MAX_LESSON_CHARS: usize = 200;
/// Lessons in effect at once. Each is in every planning request; more would
/// crowd out the task.
pub const MAX_LESSONS: usize = 20;
/// Skills are agents, and agent ids are `[a-z0-9_-]{1,32}`; this prefix
/// keeps them apart from the built-in and hand-written ones.
pub const SKILL_PREFIX: &str = "learned-";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Content {
    Lesson {
        text: String,
    },
    Skill {
        /// Without [`SKILL_PREFIX`]; the agent id is the prefix plus this.
        id: String,
        name: String,
        description: String,
        instructions: String,
        capabilities: Vec<String>,
        max_risk: RiskLevel,
    },
}

impl Content {
    pub fn kind(&self) -> &'static str {
        match self {
            Content::Lesson { .. } => "lesson",
            Content::Skill { .. } => "skill",
        }
    }

    /// The skill as the agent the butler delegates to.
    pub fn manifest(&self) -> Option<AgentManifest> {
        let Content::Skill {
            id,
            name,
            description,
            instructions,
            capabilities,
            max_risk,
        } = self
        else {
            return None;
        };
        Some(AgentManifest {
            id: format!("{SKILL_PREFIX}{id}"),
            name: name.clone(),
            description: description.clone(),
            capabilities: capabilities.clone(),
            max_risk: *max_risk,
            instructions: Some(instructions.clone()),
            max_rounds: agentos_services::agents::DEFAULT_MAX_ROUNDS,
        })
    }

    /// Whether this may take effect without a person, whatever produced it.
    /// Checked when adopting and again when loading, so a hand-edited file
    /// cannot slip in something broader.
    pub fn check(&self) -> Result<(), String> {
        match self {
            Content::Lesson { text } => {
                let n = text.trim().chars().count();
                if n == 0 || n > MAX_LESSON_CHARS {
                    return Err(format!("经验必须是 1 到 {MAX_LESSON_CHARS} 个字"));
                }
            }
            Content::Skill {
                capabilities,
                max_risk,
                ..
            } => {
                if capabilities.is_empty() {
                    return Err("技能没有能力".into());
                }
                // A skill never reaches an irreversible step, nor the
                // machinery that changes the system itself.
                if *max_risk >= RiskLevel::Irreversible {
                    return Err("技能的风险上限不能是 L3".into());
                }
                if let Some(c) = capabilities.iter().find(|c| {
                    c.ends_with('*')
                        || c.starts_with("evolve.")
                        || c.starts_with("improve.")
                        || c.starts_with(agentos_services::agents::DELEGATE_PREFIX)
                }) {
                    return Err(format!("技能不能使用 {c}"));
                }
                let manifest = self.manifest().expect("a skill");
                let mut probe = agentos_services::agents::AgentDirectory::new();
                probe.add(manifest)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// In effect, and being watched: the tasks after it are compared with
    /// the tasks before it, and it is withdrawn if they do worse.
    Observing,
    /// In effect; the watch ended without a sign of harm.
    Active,
    /// Withdrawn, by the user or automatically.
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    /// `lesson-N` or `skill-N`, never reused.
    pub id: String,
    pub content: Content,
    pub status: Status,
    /// The tasks it was learned from.
    pub sources: Vec<u64>,
    /// The model it was evaluated with; on another model it is unproven.
    pub model: Option<String>,
    /// The replay that justified it, as recorded in the audit log.
    pub evidence: Value,
    pub applied_at_ms: u64,
    /// The success rate of the user's tasks before it, to compare against.
    pub baseline: Rate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolled_back_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Item {
    pub fn in_effect(&self) -> bool {
        self.status != Status::RolledBack
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rate {
    pub tasks: usize,
    pub succeeded: usize,
}

impl Rate {
    pub fn value(&self) -> Option<f64> {
        (self.tasks > 0).then(|| self.succeeded as f64 / self.tasks as f64)
    }
}

/// A candidate that did not make it, so the same tasks are not tried again
/// and again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub at_ms: u64,
    pub sources: Vec<u64>,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Learned {
    #[serde(default)]
    pub items: Vec<Item>,
    #[serde(default)]
    pub attempts: Vec<Attempt>,
    #[serde(default)]
    pub next_id: u64,
}

/// Attempts kept; older ones are forgotten, so a task can be tried again
/// once the system has changed enough.
const MAX_ATTEMPTS: usize = 200;

impl Learned {
    pub fn lessons(&self) -> Vec<&str> {
        self.items
            .iter()
            .filter(|item| item.in_effect())
            .filter_map(|item| match &item.content {
                Content::Lesson { text } => Some(text.as_str()),
                Content::Skill { .. } => None,
            })
            .take(MAX_LESSONS)
            .collect()
    }

    pub fn skills(&self) -> Vec<AgentManifest> {
        self.items
            .iter()
            .filter(|item| item.in_effect())
            .filter_map(|item| item.content.manifest())
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|item| item.id == id)
    }

    pub fn observing(&self) -> Option<&Item> {
        self.items.iter().find(|i| i.status == Status::Observing)
    }

    /// Adopt `content`; returns its new id.
    pub fn adopt(&mut self, content: Content, mut item: Item) -> Result<String, String> {
        content.check()?;
        if let Content::Lesson { .. } = content {
            if self.lessons().len() >= MAX_LESSONS {
                return Err(format!("已经有 {MAX_LESSONS} 条经验在生效"));
            }
        }
        self.next_id += 1;
        item.id = format!("{}-{}", content.kind(), self.next_id);
        item.content = content;
        item.status = Status::Observing;
        let id = item.id.clone();
        self.items.push(item);
        Ok(id)
    }

    pub fn roll_back(&mut self, id: &str, reason: &str, now: u64) -> Result<&Item, String> {
        let item = self
            .items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or_else(|| format!("没有 {id}"))?;
        if item.status == Status::RolledBack {
            return Err(format!("{id} 已经撤回了"));
        }
        item.status = Status::RolledBack;
        item.rolled_back_at_ms = Some(now);
        item.reason = Some(reason.to_string());
        Ok(item)
    }

    pub fn remember_attempt(&mut self, attempt: Attempt) {
        self.attempts.push(attempt);
        let excess = self.attempts.len().saturating_sub(MAX_ATTEMPTS);
        self.attempts.drain(..excess);
    }

    /// Whether `task` was tried (as a source of anything) since `since_ms`.
    pub fn tried(&self, task: u64, since_ms: u64) -> bool {
        self.attempts
            .iter()
            .filter(|a| a.at_ms >= since_ms)
            .any(|a| a.sources.contains(&task))
            || self.items.iter().any(|i| i.sources.contains(&task))
    }

    fn validate(&self) -> io::Result<()> {
        for item in &self.items {
            item.content.check().map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("{}：{e}", item.id))
            })?;
        }
        Ok(())
    }
}

/// Size and modification time: enough to notice that another writer
/// replaced the file.
type Stamp = Option<(u64, SystemTime)>;

fn stamp(path: &Path) -> Stamp {
    let meta = fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

fn read(path: &Path) -> io::Result<Learned> {
    match fs::read(path) {
        Ok(bytes) => {
            let learned: Learned = serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            learned.validate()?;
            Ok(learned)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Learned::default()),
        Err(e) => Err(e),
    }
}

struct Cache {
    stamp: Stamp,
    learned: Arc<Learned>,
    generation: u64,
}

/// The learned file, re-read whenever it changes on disk.
pub struct LearnedStore {
    dir: Option<PathBuf>,
    cache: Mutex<Cache>,
}

impl LearnedStore {
    /// Read what is in `dir` (nothing yet is fine). Writes nothing, so a
    /// tool that only looks may open it beside the daemon.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        let path = dir.join(FILE);
        let learned = read(&path)?;
        Ok(Self {
            cache: Mutex::new(Cache {
                stamp: stamp(&path),
                learned: Arc::new(learned),
                generation: 1,
            }),
            dir: Some(dir),
        })
    }

    /// A store that lives only in memory, for replays and tests.
    pub fn memory(learned: Learned) -> Self {
        Self {
            dir: None,
            cache: Mutex::new(Cache {
                stamp: None,
                learned: Arc::new(learned),
                generation: 1,
            }),
        }
    }

    pub fn path(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(FILE))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// What is in effect now, and a number that changes whenever it does.
    /// A file that stopped reading back keeps the last good content: what
    /// was in effect stays in effect rather than vanishing.
    pub fn current(&self) -> (u64, Arc<Learned>) {
        let mut cache = self.lock();
        if let Some(path) = self.path() {
            let now = stamp(&path);
            if now != cache.stamp {
                match read(&path) {
                    Ok(learned) => {
                        cache.learned = Arc::new(learned);
                        cache.generation += 1;
                    }
                    Err(e) => eprintln!(
                        "学到的经验和技能（{}）读不出来，继续用上一次读到的：{e}",
                        path.display()
                    ),
                }
                cache.stamp = now;
            }
        }
        (cache.generation, cache.learned.clone())
    }

    /// Change the file under an exclusive lock, so the daemon and a rollback
    /// from elsewhere never overwrite each other. The change is durable
    /// before it is visible.
    pub fn modify<T>(
        &self,
        change: impl FnOnce(&mut Learned) -> Result<T, String>,
    ) -> io::Result<Result<T, String>> {
        let _lock = match &self.dir {
            Some(dir) => {
                agentos_runtime::create_private_dir(dir)?;
                Some(exclusive_lock(&dir.join(".lock"))?)
            }
            None => None,
        };
        let mut learned = match self.path() {
            Some(path) => read(&path)?,
            None => (*self.current().1).clone(),
        };
        let result = match change(&mut learned) {
            Ok(value) => value,
            Err(refusal) => return Ok(Err(refusal)),
        };
        if let Some(path) = self.path() {
            let bytes = serde_json::to_vec_pretty(&learned).map_err(io::Error::other)?;
            agentos_core::durable::write_atomic(&path, &bytes)?;
        }
        let mut cache = self.lock();
        cache.stamp = self.path().and_then(|p| stamp(&p));
        cache.learned = Arc::new(learned);
        cache.generation += 1;
        Ok(Ok(result))
    }
}

fn exclusive_lock(path: &Path) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    // SAFETY: `file` owns a live descriptor for the duration of the call,
    // and the lock is released when it is closed.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lesson(text: &str) -> Content {
        Content::Lesson { text: text.into() }
    }

    fn skill(capabilities: &[&str], max_risk: RiskLevel) -> Content {
        Content::Skill {
            id: "trip".into(),
            name: "出差准备".into(),
            description: "准备出差".into(),
            instructions: "先查日程再查天气".into(),
            capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
            max_risk,
        }
    }

    fn item() -> Item {
        Item {
            id: String::new(),
            content: lesson("x"),
            status: Status::Observing,
            sources: vec![3],
            model: Some("m".into()),
            evidence: json!({}),
            applied_at_ms: 1,
            baseline: Rate::default(),
            settled_at_ms: None,
            rolled_back_at_ms: None,
            reason: None,
        }
    }

    fn temp() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agentos-learned-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn what_may_take_effect_without_a_person_is_bounded() {
        // Guards "auto-applied changes never widen what the system may do":
        // no L3, no self-modification, no wildcards, no delegation.
        assert!(lesson("查天气前先确认城市").check().is_ok());
        assert!(lesson("").check().is_err());
        assert!(lesson(&"长".repeat(MAX_LESSON_CHARS + 1)).check().is_err());
        assert!(
            skill(&["calendar.read", "notes.append"], RiskLevel::Reversible)
                .check()
                .is_ok()
        );
        for bad in [
            skill(&["lock.open"], RiskLevel::Irreversible),
            skill(&["evolve.install"], RiskLevel::Reversible),
            skill(&["improve.rollback"], RiskLevel::Reversible),
            skill(&["notes.*"], RiskLevel::Reversible),
            skill(&["agent.notes"], RiskLevel::Reversible),
            skill(&[], RiskLevel::ReadOnly),
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
        let manifest = skill(&["notes.list"], RiskLevel::ReadOnly)
            .manifest()
            .unwrap();
        assert_eq!(manifest.id, "learned-trip");
    }

    #[test]
    fn lessons_are_capped_and_rolled_back_ones_stop_applying() {
        let mut learned = Learned::default();
        for i in 0..MAX_LESSONS {
            learned.adopt(lesson(&format!("经验 {i}")), item()).unwrap();
        }
        assert!(learned.adopt(lesson("再来一条"), item()).is_err());
        learned.roll_back("lesson-1", "测试", 5).unwrap();
        assert_eq!(learned.lessons().len(), MAX_LESSONS - 1);
        assert!(learned.roll_back("lesson-1", "又一次", 6).is_err());
        // The record stays, with its reason.
        assert_eq!(
            learned.get("lesson-1").unwrap().reason.as_deref(),
            Some("测试")
        );
        learned.adopt(lesson("再来一条"), item()).unwrap();
        assert_eq!(learned.items.last().unwrap().id, "lesson-21");
    }

    #[test]
    fn a_change_by_another_writer_is_picked_up_and_a_damaged_file_keeps_the_last_good() {
        let dir = temp();
        let daemon = LearnedStore::open(&dir).unwrap();
        let (g1, now) = daemon.current();
        assert!(now.items.is_empty());
        let other = LearnedStore::open(&dir).unwrap();
        other
            .modify(|l| l.adopt(lesson("先确认城市"), item()))
            .unwrap()
            .unwrap();
        let (g2, now) = daemon.current();
        assert!(g2 > g1);
        assert_eq!(now.lessons(), ["先确认城市"]);
        // A hand edit that breaks the file does not wipe what is in effect,
        // and one that sneaks in an L3 skill is refused like a damaged file.
        let mut bad: Value = serde_json::from_slice(&fs::read(dir.join(FILE)).unwrap()).unwrap();
        bad["items"][0]["content"] =
            serde_json::to_value(skill(&["x.y"], RiskLevel::Irreversible)).unwrap();
        fs::write(dir.join(FILE), serde_json::to_vec(&bad).unwrap()).unwrap();
        assert_eq!(daemon.current().1.lessons(), ["先确认城市"]);
        assert!(LearnedStore::open(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
