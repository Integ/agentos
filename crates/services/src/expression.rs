//! Expression hub: delivers what agents want to say to the right output devices.
//!
//! P0 fans out to every registered output. Later phases route by the human's
//! location, the device's modalities and the expression's urgency.

use std::sync::Arc;

use agentos_audit::{AuditEvent, AuditLog};
use agentos_core::{Expression, Output};

pub struct ExpressionHub {
    outputs: Vec<Arc<dyn Output>>,
    audit: Arc<AuditLog>,
}

impl ExpressionHub {
    pub fn new(audit: Arc<AuditLog>) -> Self {
        Self {
            outputs: Vec::new(),
            audit,
        }
    }

    pub fn add_output(&mut self, out: Arc<dyn Output>) {
        self.outputs.push(out);
    }

    pub async fn express(&self, e: Expression) {
        if let Err(err) = self.audit.append(AuditEvent::expressed(&e)) {
            eprintln!("audit write failed: {err}");
        }
        for out in &self.outputs {
            if let Err(err) = out.express(&e).await {
                eprintln!("output {} failed: {err}", out.info().id);
            }
        }
    }
}
