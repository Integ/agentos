//! AgentOS P0 runtime.
//!
//! Data flow: `Sensor` → [`perception::PerceptionHub`] → `Intent` →
//! [`butler::Butler`] (plan → policy → capability) → `Expression` →
//! [`expression::ExpressionHub`] → `Output`. Everything is audited.

pub mod builtin;
pub mod butler;
pub mod claude_planner;
pub mod devices;
pub mod evolve;
pub mod expression;
pub mod llm;
pub mod openai_planner;
pub mod perception;
pub mod planner;
pub mod plugin;
pub mod plugin_store;
pub mod registry;
pub mod shell_api;
pub mod voice;

#[cfg(test)]
mod test_http;
