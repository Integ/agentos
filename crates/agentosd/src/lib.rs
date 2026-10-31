//! AgentOS P0 runtime.
//!
//! Data flow: `Sensor` → [`perception::PerceptionHub`] → `Intent` →
//! [`butler::Butler`] (plan → policy → capability) → `Expression` →
//! [`expression::ExpressionHub`] → `Output`. Everything is audited.

pub mod builtin;
pub mod butler;
pub mod claude_planner;
pub mod devices;
pub mod expression;
pub mod openai_planner;
pub mod perception;
pub mod planner;
pub mod registry;

#[cfg(test)]
mod test_http;
