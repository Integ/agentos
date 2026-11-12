//! AgentOS system services.
//!
//! Everything a daemon needs around its task kernel, kept in one place so
//! there is exactly one implementation of each:
//!
//! - the [`registry`] of capabilities, and the [`perception`] and
//!   [`expression`] hubs with their console [`devices`];
//! - who may act: [`agents`] (manifests the butler can delegate to), the
//!   [`judge`] that can tighten a policy decision, and [`identity`] (the PIN
//!   that vouches for an irreversible step);
//! - shared memory: the [`blackboard`];
//! - capability providers: [`mcp`] servers, sandboxed [`plugin`]s with their
//!   [`plugin_store`] and the self-evolution capabilities in [`evolve`], and
//!   the smart home through [`homeassistant`].
//!
//! Nothing here decides whether a call may run. Capabilities never check
//! permissions themselves; the daemon's butler puts every call through the
//! policy engine and the audit log.

pub mod agents;
pub mod blackboard;
pub mod devices;
pub mod evolve;
pub mod expression;
pub mod homeassistant;
pub mod identity;
pub mod judge;
pub mod mcp;
pub mod perception;
pub mod plugin;
pub mod plugin_store;
pub mod registry;
