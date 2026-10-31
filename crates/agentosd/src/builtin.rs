//! Built-in capabilities for P0: a clock and an in-memory notebook.
//! The notebook spans all three risk tiers so the policy flow can be exercised.

use std::sync::{Arc, Mutex};

use agentos_core::{
    now_ms, Capability, CapabilityError, CapabilityName, CapabilitySpec, RiskLevel,
};
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

    async fn invoke(&self, _args: Value) -> Result<Value, CapabilityError> {
        let ms = now_ms();
        let secs = ms / 1000 % 86_400;
        let hms = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
        Ok(json!({"unix_ms": ms, "message": format!("现在是 UTC {hms}")}))
    }
}

pub type NoteStore = Arc<Mutex<Vec<String>>>;

fn lock(store: &NoteStore) -> std::sync::MutexGuard<'_, Vec<String>> {
    store.lock().unwrap_or_else(|p| p.into_inner())
}

pub struct NotesAppend {
    spec: CapabilitySpec,
    store: NoteStore,
}

pub struct NotesList {
    spec: CapabilitySpec,
    store: NoteStore,
}

pub struct NotesClear {
    spec: CapabilitySpec,
    store: NoteStore,
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
        let mut notes = lock(&self.store);
        notes.push(text.to_string());
        Ok(json!({"count": notes.len(), "message": format!("已记下：{text}")}))
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
        let mut notes = lock(&self.store);
        let n = notes.len();
        notes.clear();
        Ok(json!({"removed": n, "message": format!("已删除 {n} 条笔记")}))
    }
}

/// Register every built-in capability. Returns the shared note store.
pub fn register_all(reg: &mut CapabilityRegistry) -> NoteStore {
    let store: NoteStore = Arc::default();
    reg.register(Arc::new(ClockNow::default()));
    reg.register(Arc::new(NotesAppend {
        spec: spec(
            "notes.append",
            "追加一条笔记",
            RiskLevel::Reversible,
            json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
        ),
        store: store.clone(),
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
    }));
    store
}
