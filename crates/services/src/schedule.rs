//! Scheduled tasks: `schedule.create`, `schedule.list`, `schedule.cancel`.
//!
//! A schedule is an intent the user wants submitted later, once or again and
//! again. The daemon's service submits it when it is due (see
//! `agentos-taskd`), as a task from `schedule:<id>`: it is planned and
//! policed like any other, and whatever needs the human waits for the human
//! at the machine. Creating one is L1, reversible by cancelling it, and from
//! a task that read outside content it needs the human's yes like any side
//! effect, so injected text cannot quietly plant work for later.
//!
//! Runs happen at most once. A schedule is moved to its next time, durably,
//! before its task is created: a crash in between skips one run rather than
//! repeating it. A run missed by more than a day (the machine was off) is
//! skipped and says so; a later one runs once, not once per missed time.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentos_core::{
    Capability, CapabilityError, CapabilityName, CapabilitySpec, Invocation, RiskLevel,
};
use async_trait::async_trait;
use jiff::civil::{DateTime, Time};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan, Zoned};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::registry::CapabilityRegistry;

pub const FILE: &str = "schedules.json";
const MAX_SCHEDULES: usize = 50;
const MAX_TEXT_CHARS: usize = 1000;
/// A run later than this is not made up for.
const GRACE_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    Once,
    Daily,
    Weekly,
}

impl Repeat {
    fn label(self) -> &'static str {
        match self {
            Self::Once => "一次",
            Self::Daily => "每天",
            Self::Weekly => "每周",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    /// The intent, in the user's words, submitted when due.
    pub text: String,
    pub repeat: Repeat,
    /// When it is next due, Unix milliseconds.
    pub next_ms: u64,
    pub created_ms: u64,
    /// Created by a task that had read outside content. Its runs start
    /// tainted, so text an injection got past the human once cannot act
    /// unchecked every day after.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tainted: bool,
    /// What happened the last time it was due: the task it became, or why
    /// it did not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Book {
    next_id: u64,
    schedules: Vec<Schedule>,
}

/// The schedules, kept in `<state>/schedules.json`.
pub struct ScheduleStore {
    path: PathBuf,
    book: Mutex<Book>,
    zone: TimeZone,
}

impl ScheduleStore {
    /// Open the store in `dir`. A missing file is no schedules; a damaged one
    /// is an error, never quietly forgotten work.
    pub fn open(dir: &Path) -> io::Result<Self> {
        Self::open_in(dir, TimeZone::system())
    }

    fn open_in(dir: &Path, zone: TimeZone) -> io::Result<Self> {
        let path = dir.join(FILE);
        let book = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Book::default(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            path,
            book: Mutex::new(book),
            zone,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn save(&self, book: &Book) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(book).map_err(io::Error::other)?;
        agentos_core::durable::write_atomic(&self.path, &bytes)
    }

    pub fn list(&self) -> Vec<Schedule> {
        self.lock().schedules.clone()
    }

    /// A new schedule for `text` at `at` (`HH:MM`, or `YYYY-MM-DD HH:MM`),
    /// local time.
    pub fn create(
        &self,
        text: &str,
        at: &str,
        repeat: Repeat,
        now_ms: u64,
        tainted: bool,
    ) -> Result<Schedule, String> {
        let text = text.trim();
        if text.is_empty() || text.chars().count() > MAX_TEXT_CHARS {
            return Err(format!("要做的事不能为空，也不能超过 {MAX_TEXT_CHARS} 字"));
        }
        let next_ms = first_time(at, repeat, now_ms, &self.zone)?;
        let mut book = self.lock();
        if book.schedules.len() >= MAX_SCHEDULES {
            return Err(format!("最多 {MAX_SCHEDULES} 个定时任务，先取消一些"));
        }
        book.next_id += 1;
        let schedule = Schedule {
            id: format!("sched-{}", book.next_id),
            text: text.to_string(),
            repeat,
            next_ms,
            created_ms: now_ms,
            tainted,
            last: None,
        };
        book.schedules.push(schedule.clone());
        if let Err(e) = self.save(&book) {
            book.schedules.pop();
            book.next_id -= 1;
            return Err(format!("无法保存：{e}"));
        }
        Ok(schedule)
    }

    pub fn cancel(&self, id: &str) -> Result<Schedule, String> {
        let mut book = self.lock();
        let at = book
            .schedules
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| format!("没有定时任务 {id}"))?;
        let removed = book.schedules.remove(at);
        if let Err(e) = self.save(&book) {
            book.schedules.insert(at, removed);
            return Err(format!("无法保存：{e}"));
        }
        Ok(removed)
    }

    /// The schedules due at `now_ms`, already moved on to their next time
    /// (or removed) on disk. If that cannot be saved nothing is returned:
    /// a run that might repeat after a restart is worse than a late one.
    pub fn take_due(&self, now_ms: u64) -> Vec<Schedule> {
        let mut book = self.lock();
        let before = book.schedules.clone();
        let mut due = Vec::new();
        let mut keep = Vec::new();
        for mut s in std::mem::take(&mut book.schedules) {
            if s.next_ms > now_ms {
                keep.push(s);
                continue;
            }
            let missed = now_ms - s.next_ms > GRACE_MS;
            if !missed {
                due.push(s.clone());
            }
            if s.repeat == Repeat::Once {
                continue;
            }
            if missed {
                s.last = Some(format!(
                    "错过了 {}（那时服务没在运行）",
                    self.show(s.next_ms)
                ));
            }
            s.next_ms = next_after(s.next_ms, s.repeat, now_ms, &self.zone);
            keep.push(s);
        }
        book.schedules = keep;
        if due.is_empty() && book.schedules == before {
            return due;
        }
        if let Err(e) = self.save(&book) {
            eprintln!("定时任务无法保存，这一轮不运行：{e}");
            book.schedules = before;
            return Vec::new();
        }
        due
    }

    /// Note what a run became. Best effort: the run already happened.
    pub fn record(&self, id: &str, outcome: String) {
        let mut book = self.lock();
        if let Some(s) = book.schedules.iter_mut().find(|s| s.id == id) {
            s.last = Some(outcome);
            let _ = self.save(&book);
        }
    }

    fn show(&self, ms: u64) -> String {
        zoned(ms, &self.zone).strftime("%Y-%m-%d %H:%M").to_string()
    }

    fn describe(&self, s: &Schedule) -> Value {
        let mut v = json!({
            "id": s.id,
            "text": s.text,
            "repeat": s.repeat,
            "next": self.show(s.next_ms),
        });
        if let Some(last) = &s.last {
            v["last"] = json!(last);
        }
        v
    }
}

fn zoned(ms: u64, zone: &TimeZone) -> Zoned {
    Timestamp::from_millisecond(ms as i64)
        .unwrap_or(Timestamp::UNIX_EPOCH)
        .to_zoned(zone.clone())
}

fn millis(z: &Zoned) -> u64 {
    z.timestamp().as_millisecond().max(0) as u64
}

/// When a new schedule is first due.
fn first_time(at: &str, repeat: Repeat, now_ms: u64, zone: &TimeZone) -> Result<u64, String> {
    let at = at.trim();
    let bad = || format!("时间 {at} 看不懂；请写 HH:MM 或 YYYY-MM-DD HH:MM（本地时间）");
    let now = zoned(now_ms, zone);
    let (civil, date_given) = if let Ok(time) = Time::strptime("%H:%M", at) {
        (now.date().to_datetime(time), false)
    } else {
        let normal = at.replacen('T', " ", 1);
        let dt = DateTime::strptime("%Y-%m-%d %H:%M", &normal).map_err(|_| bad())?;
        (dt, true)
    };
    let first = civil.to_zoned(zone.clone()).map_err(|_| bad())?;
    let first_ms = millis(&first);
    if first_ms > now_ms {
        return Ok(first_ms);
    }
    match (repeat, date_given) {
        (Repeat::Once, true) => Err(format!("{at} 已经过去了")),
        // "08:00" once, after 8 today, means tomorrow.
        _ => Ok(next_after(
            first_ms,
            if repeat == Repeat::Once {
                Repeat::Daily
            } else {
                repeat
            },
            now_ms,
            zone,
        )),
    }
}

/// The first time after `now_ms`, stepping from `from_ms` by calendar days
/// (so 08:00 stays 08:00 across daylight-saving changes).
fn next_after(from_ms: u64, repeat: Repeat, now_ms: u64, zone: &TimeZone) -> u64 {
    let step = match repeat {
        Repeat::Weekly => 7,
        _ => 1,
    };
    let mut z = zoned(from_ms, zone);
    let start = z.datetime();
    let mut n: i64 = 0;
    while millis(&z) <= now_ms {
        n += step;
        // From the original civil time each step, so a skipped or repeated
        // hour on one day does not shift every later run.
        let civil = start.checked_add(n.days()).unwrap_or(start);
        z = civil.to_zoned(zone.clone()).unwrap_or_else(|_| z.clone());
    }
    millis(&z)
}

fn spec(name: &str, description: &str, risk: RiskLevel, schema: Value) -> CapabilitySpec {
    CapabilitySpec {
        name: CapabilityName::new(name),
        description: description.into(),
        risk,
        input_schema: schema,
    }
}

fn object<'a>(
    v: &'a Value,
    allowed: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, CapabilityError> {
    let o = v
        .as_object()
        .ok_or_else(|| CapabilityError::InvalidArgs("参数必须是对象".into()))?;
    if let Some(k) = o.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(CapabilityError::InvalidArgs(format!("不认识的参数 {k}")));
    }
    Ok(o)
}

fn string<'a>(
    o: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a str, CapabilityError> {
    o.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| CapabilityError::InvalidArgs(format!("缺少 {key}")))
}

pub fn register_all(reg: &mut CapabilityRegistry, store: Arc<ScheduleStore>) {
    reg.register(Arc::new(ScheduleCreate {
        spec: spec(
            "schedule.create",
            "安排一个定时任务：到时间后，text 会像用户说的一句话一样被执行。at 是本地时间 HH:MM 或 YYYY-MM-DD HH:MM；repeat 是 once（默认）、daily 或 weekly。有副作用的步骤到时仍要用户批准。",
            RiskLevel::Reversible,
            json!({"type": "object", "properties": {
                "text": {"type": "string", "description": "到时要做的事，用用户的话写"},
                "at": {"type": "string"},
                "repeat": {"type": "string", "enum": ["once", "daily", "weekly"]}
            }, "required": ["text", "at"], "additionalProperties": false}),
        ),
        store: store.clone(),
    }));
    reg.register(Arc::new(ScheduleList {
        spec: spec(
            "schedule.list",
            "列出所有定时任务、下次运行时间和上次的结果",
            RiskLevel::ReadOnly,
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        ),
        store: store.clone(),
    }));
    reg.register(Arc::new(ScheduleCancel {
        spec: spec(
            "schedule.cancel",
            "取消一个定时任务（id 形如 sched-3）",
            RiskLevel::Reversible,
            json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"], "additionalProperties": false}),
        ),
        store,
    }));
}

pub struct ScheduleCreate {
    spec: CapabilitySpec,
    store: Arc<ScheduleStore>,
}

#[async_trait]
impl Capability for ScheduleCreate {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        self.invoke_with(args, Invocation::default()).await
    }

    async fn invoke_with(&self, args: Value, call: Invocation) -> Result<Value, CapabilityError> {
        let o = object(&args, &["text", "at", "repeat"])?;
        let repeat = match o.get("repeat").and_then(Value::as_str) {
            None | Some("once") => Repeat::Once,
            Some("daily") => Repeat::Daily,
            Some("weekly") => Repeat::Weekly,
            Some(other) => {
                return Err(CapabilityError::InvalidArgs(format!(
                    "repeat 只能是 once、daily 或 weekly，不是 {other}"
                )))
            }
        };
        let s = self
            .store
            .create(
                string(o, "text")?,
                string(o, "at")?,
                repeat,
                agentos_core::now_ms(),
                call.tainted,
            )
            .map_err(CapabilityError::Failed)?;
        let mut out = self.store.describe(&s);
        out["message"] = json!(format!(
            "已安排（{}，{}）：{}。下次在 {}",
            s.id,
            s.repeat.label(),
            s.text,
            self.store.show(s.next_ms)
        ));
        Ok(out)
    }
}

pub struct ScheduleList {
    spec: CapabilitySpec,
    store: Arc<ScheduleStore>,
}

#[async_trait]
impl Capability for ScheduleList {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        object(&args, &[])?;
        let all = self.store.list();
        let lines: Vec<String> = all
            .iter()
            .map(|s| {
                format!(
                    "{}（{}，下次 {}）：{}",
                    s.id,
                    s.repeat.label(),
                    self.store.show(s.next_ms),
                    s.text
                )
            })
            .collect();
        Ok(json!({
            "schedules": all.iter().map(|s| self.store.describe(s)).collect::<Vec<_>>(),
            "message": if lines.is_empty() { "没有定时任务".to_string() } else { lines.join("\n") },
        }))
    }
}

pub struct ScheduleCancel {
    spec: CapabilitySpec,
    store: Arc<ScheduleStore>,
}

#[async_trait]
impl Capability for ScheduleCancel {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        let o = object(&args, &["id"])?;
        let s = self
            .store
            .cancel(string(o, "id")?)
            .map_err(CapabilityError::Failed)?;
        Ok(json!({"id": s.id, "message": format!("已取消 {}：{}", s.id, s.text)}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = 60_000;
    const DAY: u64 = 24 * 60 * MINUTE;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentos-sched-{tag}-{}-{}",
            std::process::id(),
            agentos_core::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 2026-11-21 07:00 UTC.
    fn morning() -> u64 {
        let z = DateTime::strptime("%Y-%m-%d %H:%M", "2026-11-21 07:00")
            .unwrap()
            .to_zoned(TimeZone::UTC)
            .unwrap();
        millis(&z)
    }

    #[test]
    fn times_are_read_in_local_time_and_always_lie_ahead() {
        let z = TimeZone::UTC;
        let now = morning();
        assert_eq!(
            first_time("08:00", Repeat::Once, now, &z).unwrap(),
            now + 60 * MINUTE
        );
        // Already past today: tomorrow, for one-off and daily alike.
        assert_eq!(
            first_time("06:30", Repeat::Once, now, &z).unwrap(),
            now + DAY - 30 * MINUTE
        );
        assert_eq!(
            first_time("06:30", Repeat::Daily, now, &z).unwrap(),
            now + DAY - 30 * MINUTE
        );
        assert_eq!(
            first_time("2026-11-28 07:00", Repeat::Once, now, &z).unwrap(),
            now + 7 * DAY
        );
        assert!(first_time("2026-11-20 07:00", Repeat::Once, now, &z)
            .unwrap_err()
            .contains("已经过去"));
        // A past start for a weekly one moves to its next week.
        assert_eq!(
            first_time("2026-11-20 07:00", Repeat::Weekly, now, &z).unwrap(),
            now + 6 * DAY
        );
        assert!(first_time("明天早上", Repeat::Once, now, &z).is_err());
    }

    #[test]
    fn a_run_happens_at_most_once_and_moves_on_before_it_happens() {
        let dir = temp("due");
        let store = ScheduleStore::open_in(&dir, TimeZone::UTC).unwrap();
        let now = morning();
        let daily = store
            .create("看看天气", "08:00", Repeat::Daily, now, false)
            .unwrap();
        let once = store
            .create("提醒我开会", "09:00", Repeat::Once, now, false)
            .unwrap();
        assert!(store.take_due(now).is_empty());
        let due = store.take_due(now + 2 * 60 * MINUTE + 1);
        assert_eq!(due.len(), 2);
        // Guards at-most-once: the store on disk already shows the next
        // time, so a restart right now does not run them again.
        let reopened = ScheduleStore::open_in(&dir, TimeZone::UTC).unwrap();
        assert!(reopened.take_due(now + 2 * 60 * MINUTE + 1).is_empty());
        let left = reopened.list();
        assert_eq!(left.len(), 1, "the one-off is gone");
        assert_eq!(left[0].id, daily.id);
        assert_eq!(left[0].next_ms, now + DAY + 60 * MINUTE);
        assert_ne!(once.id, daily.id);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_long_missed_run_is_skipped_and_said_so() {
        let dir = temp("missed");
        let store = ScheduleStore::open_in(&dir, TimeZone::UTC).unwrap();
        let now = morning();
        let s = store
            .create("看看天气", "08:00", Repeat::Daily, now, false)
            .unwrap();
        // Off for three days: no burst of make-up runs, and an honest note.
        let later = now + 3 * DAY + 2 * 60 * MINUTE;
        assert!(store.take_due(later).is_empty());
        let after = &store.list()[0];
        assert_eq!(after.id, s.id);
        assert!(after.last.as_deref().unwrap().contains("错过"));
        assert!(after.next_ms > later);
        assert_eq!(store.cancel(&s.id).unwrap().id, s.id);
        assert!(store.list().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn capabilities_create_list_and_cancel() {
        let dir = temp("caps");
        let mut reg = CapabilityRegistry::new();
        register_all(&mut reg, Arc::new(ScheduleStore::open(&dir).unwrap()));
        let cap = |n: &str| reg.get(&CapabilityName::new(n)).unwrap().clone();
        // Guards honest risk: planting future work is a side effect.
        assert_eq!(cap("schedule.create").spec().risk, RiskLevel::Reversible);
        let made = cap("schedule.create")
            .invoke(json!({"text": "提醒我喝水", "at": "2099-01-01 08:00"}))
            .await
            .unwrap();
        assert_eq!(made["id"], "sched-1");
        let list = cap("schedule.list").invoke(json!({})).await.unwrap();
        assert!(list["message"].as_str().unwrap().contains("提醒我喝水"));
        assert!(cap("schedule.create")
            .invoke(json!({"text": "x", "at": "08:00", "repeat": "hourly"}))
            .await
            .is_err());
        cap("schedule.cancel")
            .invoke(json!({"id": "sched-1"}))
            .await
            .unwrap();
        assert!(cap("schedule.cancel")
            .invoke(json!({"id": "sched-1"}))
            .await
            .is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
