//! AgentOS task runtime.
//!
//! Data flow: `Sensor` → [`perception::PerceptionHub`] → `Intent` →
//! [`butler::Butler`] (plan → policy → capability) → `Expression` →
//! [`expression::ExpressionHub`] → `Output`. Everything is audited.

pub mod builtin;
pub mod butler;
pub mod callers;
pub mod control;
pub mod crash;
pub mod mic_led;
pub mod migrate;
pub mod preauth;
pub mod service;
pub mod voice;
