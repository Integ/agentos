//! Policy engine: decides whether an agent may invoke a capability.
//!
//! Two independent checks:
//! 1. **Grant** (object-capability style): the agent must hold a grant for the
//!    capability, issued from its manifest at install time. No grant, no call.
//! 2. **Risk tier**: the harder an action is to undo, the more a human must be
//!    involved. Context derived from untrusted input (web pages, email, ...) is
//!    *tainted* and raises the bar, so a prompt injection cannot silently
//!    trigger side effects.

use std::collections::{HashMap, HashSet};

use agentos_core::{AgentId, CapabilityName, CapabilitySpec, RiskLevel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Ask the nearest human first. `verify_identity` means the confirmation
    /// must come with identity proof (voiceprint, face, PIN) — enforced by the
    /// confirming device, not the agent.
    NeedsConfirmation {
        reason: String,
        verify_identity: bool,
    },
    Deny {
        reason: String,
    },
}

/// Per-call facts the engine needs beyond the capability spec.
#[derive(Debug, Clone, Copy, Default)]
pub struct CallContext {
    /// The plan was influenced by untrusted external content.
    pub tainted: bool,
}

#[derive(Debug, Default)]
pub struct PolicyEngine {
    grants: HashMap<AgentId, HashSet<CapabilityName>>,
    /// Capabilities the human pre-approved for an agent ("always allow"),
    /// skipping confirmation for L1/L2. Never applies to L3.
    preapproved: HashMap<AgentId, HashSet<CapabilityName>>,
}

impl PolicyEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn grant(&mut self, agent: &AgentId, cap: &CapabilityName) {
        self.grants
            .entry(agent.clone())
            .or_default()
            .insert(cap.clone());
    }

    pub fn revoke(&mut self, agent: &AgentId, cap: &CapabilityName) {
        if let Some(set) = self.grants.get_mut(agent) {
            set.remove(cap);
        }
        if let Some(set) = self.preapproved.get_mut(agent) {
            set.remove(cap);
        }
    }

    pub fn preapprove(&mut self, agent: &AgentId, cap: &CapabilityName) {
        self.preapproved
            .entry(agent.clone())
            .or_default()
            .insert(cap.clone());
    }

    pub fn is_granted(&self, agent: &AgentId, cap: &CapabilityName) -> bool {
        self.grants.get(agent).is_some_and(|s| s.contains(cap))
    }

    fn is_preapproved(&self, agent: &AgentId, cap: &CapabilityName) -> bool {
        self.preapproved.get(agent).is_some_and(|s| s.contains(cap))
    }

    pub fn check(&self, agent: &AgentId, spec: &CapabilitySpec, ctx: CallContext) -> Decision {
        if !self.is_granted(agent, &spec.name) {
            return Decision::Deny {
                reason: format!("{agent} 没有 {} 的授权", spec.name),
            };
        }
        let pre = self.is_preapproved(agent, &spec.name);
        match spec.risk {
            RiskLevel::ReadOnly => Decision::Allow,
            RiskLevel::Reversible if !ctx.tainted => Decision::Allow,
            RiskLevel::Reversible | RiskLevel::External if pre && !ctx.tainted => Decision::Allow,
            RiskLevel::Reversible => Decision::NeedsConfirmation {
                reason: "该操作受不可信的外部内容影响".into(),
                verify_identity: false,
            },
            RiskLevel::External => Decision::NeedsConfirmation {
                reason: "该操作会把数据发出本机或产生费用".into(),
                verify_identity: false,
            },
            RiskLevel::Irreversible => Decision::NeedsConfirmation {
                reason: "该操作不可撤销或涉及安全".into(),
                verify_identity: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str, risk: RiskLevel) -> CapabilitySpec {
        CapabilitySpec {
            name: CapabilityName::new(name),
            description: String::new(),
            risk,
            input_schema: json!({}),
        }
    }

    fn engine_with(agent: &AgentId, caps: &[&CapabilitySpec]) -> PolicyEngine {
        let mut e = PolicyEngine::new();
        for c in caps {
            e.grant(agent, &c.name);
        }
        e
    }

    #[test]
    fn ungranted_is_denied_even_if_read_only() {
        let a = AgentId::new("butler");
        let s = spec("clock.now", RiskLevel::ReadOnly);
        let e = PolicyEngine::new();
        assert!(matches!(
            e.check(&a, &s, CallContext::default()),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn tiers_map_to_decisions() {
        let a = AgentId::new("butler");
        let l0 = spec("l0", RiskLevel::ReadOnly);
        let l1 = spec("l1", RiskLevel::Reversible);
        let l2 = spec("l2", RiskLevel::External);
        let l3 = spec("l3", RiskLevel::Irreversible);
        let e = engine_with(&a, &[&l0, &l1, &l2, &l3]);
        let clean = CallContext::default();
        assert_eq!(e.check(&a, &l0, clean), Decision::Allow);
        assert_eq!(e.check(&a, &l1, clean), Decision::Allow);
        assert!(matches!(
            e.check(&a, &l2, clean),
            Decision::NeedsConfirmation {
                verify_identity: false,
                ..
            }
        ));
        assert!(matches!(
            e.check(&a, &l3, clean),
            Decision::NeedsConfirmation {
                verify_identity: true,
                ..
            }
        ));
    }

    #[test]
    fn taint_forces_confirmation_for_side_effects() {
        let a = AgentId::new("butler");
        let l1 = spec("l1", RiskLevel::Reversible);
        let mut e = engine_with(&a, &[&l1]);
        e.preapprove(&a, &l1.name);
        let tainted = CallContext { tainted: true };
        assert!(matches!(
            e.check(&a, &l1, tainted),
            Decision::NeedsConfirmation { .. }
        ));
    }

    #[test]
    fn preapproval_never_covers_irreversible() {
        let a = AgentId::new("butler");
        let l2 = spec("l2", RiskLevel::External);
        let l3 = spec("l3", RiskLevel::Irreversible);
        let mut e = engine_with(&a, &[&l2, &l3]);
        e.preapprove(&a, &l2.name);
        e.preapprove(&a, &l3.name);
        assert_eq!(e.check(&a, &l2, CallContext::default()), Decision::Allow);
        assert!(matches!(
            e.check(&a, &l3, CallContext::default()),
            Decision::NeedsConfirmation { .. }
        ));
    }

    #[test]
    fn revoke_removes_grant() {
        let a = AgentId::new("butler");
        let l0 = spec("l0", RiskLevel::ReadOnly);
        let mut e = engine_with(&a, &[&l0]);
        e.revoke(&a, &l0.name);
        assert!(matches!(
            e.check(&a, &l0, CallContext::default()),
            Decision::Deny { .. }
        ));
    }
}
