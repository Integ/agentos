//! AgentOS P0 runtime.
//!
//! Data flow: `Sensor` → [`perception::PerceptionHub`] → `Intent` →
//! [`butler::Butler`] (plan → policy → capability) → `Expression` →
//! [`expression::ExpressionHub`] → `Output`. Everything is audited.

pub mod agents;
pub mod blackboard;
pub mod builtin;
pub mod butler;
pub mod claude_planner;
pub mod crash;
pub mod devices;
pub mod evolve;
pub mod expression;
pub mod homeassistant;
pub mod identity;
pub mod judge;
pub mod llm;
pub mod mcp;
pub mod mic_led;
pub mod openai_planner;
pub mod perception;
pub mod planner;
pub mod plugin;
pub mod plugin_store;
pub mod registry;
pub mod shell_api;
pub mod tasks;
pub mod voice;

#[cfg(test)]
mod test_http;
