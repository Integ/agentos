//! AgentOS P0 runtime.
//!
//! Data flow: `Sensor` → [`perception::PerceptionHub`] → `Intent` →
//! [`butler::Butler`] (plan → policy → capability) → `Expression` →
//! [`expression::ExpressionHub`] → `Output`. Everything is audited.

pub mod builtin;
pub mod butler;
pub mod crash;
pub mod mic_led;
pub mod shell_api;
pub mod tasks;
pub mod voice;
