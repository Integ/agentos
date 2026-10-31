//! Capability registry: the set of tools available on this machine.

use std::collections::BTreeMap;
use std::sync::Arc;

use agentos_core::{Capability, CapabilityName, CapabilitySpec};

#[derive(Default, Clone)]
pub struct CapabilityRegistry {
    caps: BTreeMap<CapabilityName, Arc<dyn Capability>>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a capability. Returns the previous one with the same name, if any.
    pub fn register(&mut self, cap: Arc<dyn Capability>) -> Option<Arc<dyn Capability>> {
        self.caps.insert(cap.spec().name.clone(), cap)
    }

    pub fn get(&self, name: &CapabilityName) -> Option<&Arc<dyn Capability>> {
        self.caps.get(name)
    }

    pub fn specs(&self) -> Vec<CapabilitySpec> {
        self.caps.values().map(|c| c.spec().clone()).collect()
    }
}
