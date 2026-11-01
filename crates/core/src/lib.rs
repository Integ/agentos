//! AgentOS core types.
//!
//! The vocabulary every other component speaks:
//! - [`Percept`]: a timestamped event produced by an input device (a [`Sensor`]).
//! - [`Intent`]: a structured human goal, derived from one or more percepts.
//! - [`Capability`]: a typed, callable tool an agent may invoke if it holds a grant.
//! - [`Expression`]: something an agent wants to tell the human; the expression hub
//!   decides which [`Output`] device delivers it.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(AgentId);
string_id!(DeviceId);
string_id!(CapabilityName);

/// Monotonic per-process intent id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IntentId(pub u64);

impl fmt::Display for IntentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "intent-{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Devices, percepts, expressions
// ---------------------------------------------------------------------------

/// How an output device reaches the human.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Visual,
    Audio,
    Haptic,
    Ambient,
}

/// How sensitive the raw data from a device is. Raw data from `Private`
/// devices (camera, microphone) never leaves the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyClass {
    Public,
    Personal,
    Private,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub kind: String,
    pub location: Option<String>,
    pub privacy: PrivacyClass,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PerceptKind {
    /// Typed text, or a speech transcript.
    Text {
        text: String,
        speaker: Option<String>,
    },
    /// Someone entered or left the device's field of view / hearing.
    Presence { present: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Percept {
    pub device: DeviceId,
    pub at_ms: u64,
    pub kind: PerceptKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    Low,
    Normal,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    pub id: IntentId,
    pub source: DeviceId,
    pub speaker: Option<String>,
    pub text: String,
    pub urgency: Urgency,
    pub at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpressionKind {
    /// A progress update or result.
    Report,
    /// A plan the agent is about to carry out.
    Plan,
    /// A request for the human to confirm an action.
    AskConfirm,
    /// Something went wrong.
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Expression {
    pub from: AgentId,
    pub intent: Option<IntentId>,
    pub kind: ExpressionKind,
    pub urgency: Urgency,
    pub text: String,
}

/// An input device. Produces percepts until it is exhausted.
#[async_trait]
pub trait Sensor: Send {
    fn info(&self) -> DeviceInfo;
    /// Next percept, or `None` when the device is closed.
    async fn next(&mut self) -> Option<Percept>;
}

/// An output device.
#[async_trait]
pub trait Output: Send + Sync {
    fn info(&self) -> DeviceInfo;
    fn modalities(&self) -> &[Modality];
    async fn express(&self, e: &Expression) -> Result<(), OutputError>;
}

#[derive(Debug, thiserror::Error)]
#[error("output device failed: {0}")]
pub struct OutputError(pub String);

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// How hard an action is to undo. Drives the default confirmation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// L0: read-only, local.
    ReadOnly,
    /// L1: local and reversible.
    Reversible,
    /// L2: leaves the machine, or spends money.
    External,
    /// L3: irreversible or safety-relevant.
    Irreversible,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitySpec {
    pub name: CapabilityName,
    pub description: String,
    pub risk: RiskLevel,
    /// JSON Schema of the arguments (MCP-compatible).
    pub input_schema: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum CapabilityError {
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("capability failed: {0}")]
    Failed(String),
}

/// A callable tool. Implementations must not perform any permission checks
/// themselves; the policy engine gates every call.
#[async_trait]
pub trait Capability: Send + Sync {
    fn spec(&self) -> &CapabilitySpec;
    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError>;

    /// The result may carry content from outside the user's control (a web
    /// page, an email, anything fetched from the network). Once an agent has
    /// seen it, its later steps are *tainted*: the policy engine asks the human
    /// before any side effect, so injected instructions cannot act silently.
    fn untrusted_output(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_levels_are_ordered() {
        assert!(RiskLevel::ReadOnly < RiskLevel::Reversible);
        assert!(RiskLevel::External < RiskLevel::Irreversible);
    }

    #[test]
    fn percept_serializes_with_tag() {
        let p = PerceptKind::Text {
            text: "hi".into(),
            speaker: None,
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["type"], "text");
    }
}
