//! The blackboard: one shared, durable JSON document every task and agent can
//! read and write — the agents' shared memory.
//!
//! Agents hand facts to each other here instead of through their (private)
//! conversations. Every write is one atomic path operation and bumps a
//! revision; a writer may pass the revision it read, and a stale one is
//! rejected rather than silently overwriting someone else's change. The
//! document is saved to disk after every write (write to a temp file, then
//! rename), so it survives restarts.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentos_core::{Capability, CapabilityError, CapabilityName, CapabilitySpec, RiskLevel};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::registry::CapabilityRegistry;

/// Largest document the board accepts, serialized.
pub const MAX_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Saved {
    rev: u64,
    doc: Value,
}

#[derive(Debug)]
struct Inner {
    path: Option<PathBuf>,
    rev: u64,
    doc: Value,
}

/// Cheap to clone; all clones share one document.
#[derive(Debug, Clone)]
pub struct Blackboard(Arc<Mutex<Inner>>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Set,
    Delete,
    Increment,
    Append,
}

fn segments(path: &str) -> Vec<&str> {
    path.split(['/', '.'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

impl Blackboard {
    /// A board kept only in memory.
    pub fn in_memory() -> Self {
        Self(Arc::new(Mutex::new(Inner {
            path: None,
            rev: 0,
            doc: json!({}),
        })))
    }

    /// Open (or create) the board stored at `path`.
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let saved: Saved = match std::fs::read_to_string(&path) {
            Ok(t) => serde_json::from_str(&t)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Saved {
                rev: 0,
                doc: json!({}),
            },
            Err(e) => return Err(e),
        };
        Ok(Self(Arc::new(Mutex::new(Inner {
            path: Some(path),
            rev: saved.rev,
            doc: if saved.doc.is_object() {
                saved.doc
            } else {
                json!({})
            },
        }))))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn rev(&self) -> u64 {
        self.lock().rev
    }

    /// The value at `path` (the whole board for an empty path) and the revision.
    pub fn read(&self, path: &str) -> (Option<Value>, u64) {
        let g = self.lock();
        let mut cur = &g.doc;
        for s in segments(path) {
            match cur.get(s) {
                Some(v) => cur = v,
                None => return (None, g.rev),
            }
        }
        (Some(cur.clone()), g.rev)
    }

    /// Apply one operation. `expect_rev` guards against lost updates.
    pub fn write(
        &self,
        op: Op,
        path: &str,
        value: Option<Value>,
        expect_rev: Option<u64>,
    ) -> Result<u64, String> {
        let segs = segments(path);
        let Some((last, parents)) = segs.split_last() else {
            return Err("path 不能为空".into());
        };
        let mut g = self.lock();
        if let Some(r) = expect_rev {
            if r != g.rev {
                return Err(format!(
                    "黑板已被改动（你读到的是版本 {r}，现在是 {}），请重新读取后再写",
                    g.rev
                ));
            }
        }
        let mut doc = g.doc.clone();
        let mut cur = &mut doc;
        for s in parents {
            let obj = cur
                .as_object_mut()
                .ok_or_else(|| format!("{s} 的上一级不是对象"))?;
            cur = obj
                .entry(s.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
        }
        let obj = cur
            .as_object_mut()
            .ok_or_else(|| format!("{last} 的上一级不是对象"))?;
        match op {
            Op::Set => {
                obj.insert(last.to_string(), value.ok_or("set 需要 value")?);
            }
            Op::Delete => {
                obj.remove(*last);
            }
            Op::Increment => {
                let by = value.as_ref().and_then(Value::as_f64).unwrap_or(1.0);
                let old = match obj.get(*last) {
                    None => 0.0,
                    Some(v) => v.as_f64().ok_or("increment 只能用于数字")?,
                };
                let n = old + by;
                let v = if n.fract() == 0.0 && n.abs() < 9e15 {
                    json!(n as i64)
                } else {
                    json!(n)
                };
                obj.insert(last.to_string(), v);
            }
            Op::Append => {
                let v = value.ok_or("append 需要 value")?;
                let slot = obj
                    .entry(last.to_string())
                    .or_insert_with(|| Value::Array(Vec::new()));
                slot.as_array_mut().ok_or("append 只能用于列表")?.push(v);
            }
        }
        let text = serde_json::to_string(&doc).map_err(|e| e.to_string())?;
        if text.len() > MAX_BYTES {
            return Err(format!("黑板超过 {} KB 上限", MAX_BYTES / 1024));
        }
        let rev = g.rev + 1;
        if let Some(p) = &g.path {
            save(
                p,
                &Saved {
                    rev,
                    doc: doc.clone(),
                },
            )
            .map_err(|e| format!("保存黑板失败：{e}"))?;
        }
        g.doc = doc;
        g.rev = rev;
        Ok(rev)
    }

    /// A short rendering for a planner's context.
    pub fn summary(&self, max_chars: usize) -> Option<String> {
        let g = self.lock();
        if g.doc.as_object().map_or(true, Map::is_empty) {
            return None;
        }
        let mut s = g.doc.to_string();
        if s.chars().count() > max_chars {
            s = s.chars().take(max_chars).collect::<String>()
                + "…（已截断，用 board__read 读取完整内容）";
        }
        Some(format!("版本 {}：{s}", g.rev))
    }
}

pub(crate) fn save<T: Serialize>(path: &Path, v: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    std::fs::rename(tmp, path)
}

struct BoardRead {
    spec: CapabilitySpec,
    board: Blackboard,
}

#[async_trait]
impl Capability for BoardRead {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
        let (value, rev) = self.board.read(path);
        let message = match &value {
            Some(v) => format!(
                "黑板 {}（版本 {rev}）：{v}",
                if path.is_empty() { "/" } else { path }
            ),
            None => format!("黑板上没有 {path}（版本 {rev}）"),
        };
        Ok(json!({"path": path, "value": value, "rev": rev, "message": message}))
    }
}

struct BoardWrite {
    spec: CapabilitySpec,
    board: Blackboard,
}

#[async_trait]
impl Capability for BoardWrite {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        let op: Op = serde_json::from_value(args.get("op").cloned().unwrap_or(json!("set")))
            .map_err(|_| {
                CapabilityError::InvalidArgs("op 只能是 set、delete、increment、append".into())
            })?;
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::InvalidArgs("缺少 path".into()))?;
        let rev = self
            .board
            .write(
                op,
                path,
                args.get("value").cloned(),
                args.get("rev").and_then(Value::as_u64),
            )
            .map_err(CapabilityError::Failed)?;
        Ok(json!({"rev": rev, "message": format!("已写入黑板 {path}（版本 {rev}）")}))
    }
}

/// Register `board.read` and `board.write`.
pub fn register_all(reg: &mut CapabilityRegistry, board: Blackboard) {
    reg.register(Arc::new(BoardRead {
        spec: CapabilitySpec {
            name: CapabilityName::new("board.read"),
            description: "读取所有 Agent 共享的黑板（持久保存的 JSON）。path 用 / 分隔，留空读取全部".into(),
            risk: RiskLevel::ReadOnly,
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string", "description": "例如 shopping/today，留空读全部"}}
            }),
        },
        board: board.clone(),
    }));
    reg.register(Arc::new(BoardWrite {
        spec: CapabilitySpec {
            name: CapabilityName::new("board.write"),
            description: "在共享黑板上记下事实，交给以后的任务或其他 Agent。一次一个操作：set 设置、delete 删除、increment 计数、append 追加到列表。传入读到的 rev 可以防止覆盖别人的改动".into(),
            risk: RiskLevel::Reversible,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["set", "delete", "increment", "append"]},
                    "path": {"type": "string"},
                    "value": {},
                    "rev": {"type": "integer", "description": "读到的版本号；黑板已变化时写入会被拒绝"}
                },
                "required": ["op", "path"]
            }),
        },
        board,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops_and_revisions() {
        let b = Blackboard::in_memory();
        assert_eq!(
            b.write(Op::Set, "trip/city", Some(json!("杭州")), None),
            Ok(1)
        );
        assert_eq!(b.write(Op::Increment, "trip/days", None, Some(1)), Ok(2));
        assert_eq!(
            b.write(Op::Increment, "trip/days", Some(json!(2)), None),
            Ok(3)
        );
        assert_eq!(
            b.write(Op::Append, "trip/todo", Some(json!("订酒店")), None),
            Ok(4)
        );
        assert_eq!(
            b.read("trip").0.unwrap(),
            json!({"city": "杭州", "days": 3, "todo": ["订酒店"]})
        );
        // A writer holding an old revision is refused.
        assert!(b.write(Op::Delete, "trip/city", None, Some(1)).is_err());
        assert_eq!(b.write(Op::Delete, "trip/city", None, Some(4)), Ok(5));
        assert_eq!(b.read("trip/city").0, None);
        assert!(b.write(Op::Increment, "trip/todo", None, None).is_err());
        assert!(b.write(Op::Set, "", Some(json!(1)), None).is_err());
    }

    #[test]
    fn survives_reopen() {
        let p = std::env::temp_dir().join(format!("agentos-board-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let b = Blackboard::open(&p).unwrap();
        b.write(Op::Set, "a", Some(json!(1)), None).unwrap();
        let b = Blackboard::open(&p).unwrap();
        assert_eq!(b.read("a"), (Some(json!(1)), 1));
        assert!(b.summary(100).unwrap().contains("\"a\":1"));
        let _ = std::fs::remove_file(&p);
    }
}
