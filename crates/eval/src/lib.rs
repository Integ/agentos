//! Evaluations of AgentOS that need the whole task kernel but must not touch
//! anything real. They live outside the daemon on purpose: what they build to
//! compare against (a policy without taint) must never be something the
//! product can be configured to do.

pub mod injection;
