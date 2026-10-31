//! Perception hub: turns raw percepts into intents.
//!
//! P0 only handles text percepts (keyboard, or a speech transcript). Later
//! phases fuse several percepts (speech + presence + gaze) into one intent.

use agentos_core::{Intent, IntentId, Percept, PerceptKind, Urgency};

#[derive(Debug, Default)]
pub struct PerceptionHub {
    next_id: u64,
}

impl PerceptionHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn to_intent(&mut self, p: Percept) -> Option<Intent> {
        match p.kind {
            PerceptKind::Text { text, speaker } => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                self.next_id += 1;
                Some(Intent {
                    id: IntentId(self.next_id),
                    source: p.device,
                    speaker,
                    urgency: if text.ends_with('!') || text.ends_with('！') {
                        Urgency::High
                    } else {
                        Urgency::Normal
                    },
                    text,
                    at_ms: p.at_ms,
                })
            }
            PerceptKind::Presence { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::{now_ms, DeviceId};

    fn text(t: &str) -> Percept {
        Percept {
            device: DeviceId::new("kbd"),
            at_ms: now_ms(),
            kind: PerceptKind::Text {
                text: t.into(),
                speaker: None,
            },
        }
    }

    #[test]
    fn blank_text_is_ignored_and_ids_increase() {
        let mut h = PerceptionHub::new();
        assert!(h.to_intent(text("   ")).is_none());
        let a = h.to_intent(text("hi")).unwrap();
        let b = h.to_intent(text("急！")).unwrap();
        assert_eq!(a.id, IntentId(1));
        assert_eq!(b.id, IntentId(2));
        assert_eq!(b.urgency, Urgency::High);
    }
}
