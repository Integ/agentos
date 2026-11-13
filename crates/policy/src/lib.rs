//! Policy engine: decides whether an agent may invoke a capability.
//!
//! Two independent checks:
//! 1. **Grant** (object-capability style): the agent must hold a grant for the
//!    capability, issued from its manifest at install time. No grant, no call.
//! 2. **Risk tier**: the harder an action is to undo, the more a human must be
//!    involved. Context derived from untrusted input (web pages, email, ...) is
//!    *tainted* and raises the bar, so a prompt injection cannot silently
//!    trigger side effects.
//!
//! A human may lift the confirmation for a narrow class of calls in advance
//! with a [`Preauthorization`]: one capability, rules its arguments must
//! satisfy, and optionally an expiry. It never lifts a denial, never applies
//! to an irreversible call, and never applies once the context is tainted.

use std::collections::{HashMap, HashSet};

use agentos_core::{AgentId, CapabilityName, CapabilitySpec, RiskLevel};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// Whose tasks a pre-authorization applies to. One made for the user's own
/// tasks never covers anyone else's, and the other way round.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Subject {
    /// Tasks the user submitted themselves.
    #[default]
    User,
    /// Tasks submitted by one registered outside caller.
    Caller(String),
}

/// What one argument of a call must look like.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Matcher {
    /// Exactly this value.
    Equals(Value),
    /// One of these values. An array argument matches when every element is
    /// one of them (recipients, for example).
    OneOf(Vec<Value>),
    /// A string that starts with this text. Plain text comparison: use
    /// [`Matcher::Under`] for file paths.
    Prefix(String),
    /// A path inside this directory (or the directory itself). The argument
    /// must not contain `.` or `..` components, so it cannot climb out.
    Under(String),
    /// Anything. States that the argument was considered and left free.
    Any,
}

impl Matcher {
    fn accepts(&self, value: &Value) -> bool {
        // For rules about text, an array of texts passes when all of it does.
        let each = |accept: &dyn Fn(&str) -> bool| match value {
            Value::String(text) => accept(text),
            Value::Array(items) => {
                !items.is_empty() && items.iter().all(|item| item.as_str().is_some_and(accept))
            }
            _ => false,
        };
        match self {
            Self::Any => true,
            Self::Equals(wanted) => value == wanted,
            Self::OneOf(allowed) => match value {
                Value::Array(items) => {
                    !items.is_empty() && items.iter().all(|item| allowed.contains(item))
                }
                single => allowed.contains(single),
            },
            Self::Prefix(prefix) => each(&|text| text.starts_with(prefix.as_str())),
            Self::Under(directory) => each(&|text| path_is_under(text, directory)),
        }
    }

    /// `Any` constrains nothing.
    fn constrains(&self) -> bool {
        !matches!(self, Self::Any)
    }
}

/// Lexical containment, with no access to the file system: `path` names
/// `directory` or something below it, and has no component that could step
/// back out.
fn path_is_under(path: &str, directory: &str) -> bool {
    let parts = |text: &str| -> Option<Vec<String>> {
        let parts: Vec<String> = text
            .split('/')
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect();
        (!parts.iter().any(|part| part == "." || part == "..")).then_some(parts)
    };
    let (Some(path_parts), Some(directory_parts)) = (parts(path), parts(directory)) else {
        return false;
    };
    path.starts_with('/') == directory.starts_with('/')
        && path_parts.len() >= directory_parts.len()
        && path_parts[..directory_parts.len()] == directory_parts[..]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArgRule {
    /// A top-level argument of the call.
    pub field: String,
    #[serde(rename = "match")]
    pub matcher: Matcher,
}

/// Standing permission, given by the human in advance, for calls of one
/// capability whose arguments satisfy every rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preauthorization {
    pub id: String,
    #[serde(default)]
    pub subject: Subject,
    pub capability: CapabilityName,
    pub args: Vec<ArgRule>,
    /// Whether arguments no rule mentions are acceptable. Off by default: an
    /// argument nobody looked at (a `bcc`, an `overwrite`) is not approved.
    #[serde(default)]
    pub other_args: bool,
    pub created_at_ms: u64,
    #[serde(default)]
    pub expires_at_ms: Option<u64>,
}

impl Preauthorization {
    /// Why this could not be a pre-authorization, if it could not. It must
    /// narrow something: be limited in time, or constrain an argument.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = HashSet::new();
        for rule in &self.args {
            if rule.field.is_empty() || !seen.insert(rule.field.as_str()) {
                return Err(format!("参数 {:?} 为空或重复", rule.field));
            }
            match &rule.matcher {
                Matcher::OneOf(allowed) if allowed.is_empty() => {
                    return Err(format!("参数 {} 的可选值不能为空", rule.field));
                }
                Matcher::Prefix(text) | Matcher::Under(text) if text.is_empty() => {
                    return Err(format!("参数 {} 的前缀或目录不能为空", rule.field));
                }
                Matcher::Under(directory) if !path_is_under(directory, directory) => {
                    return Err(format!("参数 {} 的目录不能包含 . 或 ..", rule.field));
                }
                _ => {}
            }
        }
        let constrained = self.args.iter().any(|rule| rule.matcher.constrains());
        if !constrained && self.expires_at_ms.is_none() {
            return Err("预授权必须限定至少一个参数，或者设置期限".into());
        }
        Ok(())
    }

    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_some_and(|at| now_ms >= at)
    }

    /// Whether a call of `capability` with `args` for `subject` is one this
    /// pre-authorization was given for.
    pub fn covers(
        &self,
        subject: &Subject,
        capability: &CapabilityName,
        args: &Value,
        now_ms: u64,
    ) -> bool {
        if &self.subject != subject || &self.capability != capability || self.is_expired(now_ms) {
            return false;
        }
        let Some(args) = args.as_object() else {
            return false;
        };
        let ruled = self.args.iter().all(|rule| {
            args.get(&rule.field)
                .is_some_and(|value| rule.matcher.accepts(value))
        });
        let stray = args
            .keys()
            .any(|name| !self.args.iter().any(|rule| &rule.field == name));
        ruled && (self.other_args || !stray)
    }
}

/// The human's standing permissions as they are at the moment of a check:
/// whose task the call belongs to, the rules in force, and the time.
#[derive(Debug, Clone, Copy)]
pub struct Standing<'a> {
    pub subject: &'a Subject,
    pub preauthorizations: &'a [Preauthorization],
    pub now_ms: u64,
}

/// The outcome of checking one call, and the pre-authorization that lifted
/// its confirmation, if one did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub decision: Decision,
    pub preauthorization: Option<String>,
}

#[derive(Debug, Default)]
pub struct PolicyEngine {
    grants: HashMap<AgentId, HashSet<CapabilityName>>,
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
    }

    /// Replace everything `agent` holds with exactly `caps`.
    pub fn set_grants(&mut self, agent: &AgentId, caps: impl IntoIterator<Item = CapabilityName>) {
        self.grants
            .insert(agent.clone(), caps.into_iter().collect());
    }

    pub fn is_granted(&self, agent: &AgentId, cap: &CapabilityName) -> bool {
        self.grants.get(agent).is_some_and(|s| s.contains(cap))
    }

    pub fn check(&self, agent: &AgentId, spec: &CapabilitySpec, ctx: CallContext) -> Decision {
        if !self.is_granted(agent, &spec.name) {
            return Decision::Deny {
                reason: format!("{agent} 没有 {} 的授权", spec.name),
            };
        }
        match spec.risk {
            RiskLevel::ReadOnly => Decision::Allow,
            RiskLevel::Reversible if !ctx.tainted => Decision::Allow,
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

impl PolicyEngine {
    /// [`Self::check`] for one concrete call, taking into account whose task
    /// it belongs to and the human's standing pre-authorizations.
    ///
    /// A task submitted by an outside caller has no human behind its words,
    /// so anything it does beyond reading asks the human first, where the
    /// user's own reversible calls would simply run.
    ///
    /// Pre-authorizations are looked at last and can only
    /// do one thing: turn "ask first" into "allow" for a reversible or
    /// external call in an untainted context. A denial stays a denial, an
    /// irreversible call still needs the human and their identity, and a
    /// tainted context still asks, because what the call was planned from is
    /// not what the human pre-authorized.
    pub fn check_call(
        &self,
        agent: &AgentId,
        spec: &CapabilitySpec,
        args: &Value,
        ctx: CallContext,
        standing: Standing<'_>,
    ) -> Checked {
        let decision = match (self.check(agent, spec, ctx), standing.subject) {
            (Decision::Allow, Subject::Caller(caller)) if spec.risk > RiskLevel::ReadOnly => {
                Decision::NeedsConfirmation {
                    reason: format!("外部调用方 {caller} 请求执行有副作用的操作"),
                    verify_identity: false,
                }
            }
            (decision, _) => decision,
        };
        let liftable = matches!(
            decision,
            Decision::NeedsConfirmation {
                verify_identity: false,
                ..
            }
        ) && !ctx.tainted
            && matches!(spec.risk, RiskLevel::Reversible | RiskLevel::External);
        let used = liftable
            .then(|| {
                standing
                    .preauthorizations
                    .iter()
                    .find(|pre| pre.covers(standing.subject, &spec.name, args, standing.now_ms))
            })
            .flatten();
        match used {
            Some(pre) => Checked {
                decision: Decision::Allow,
                preauthorization: Some(pre.id.clone()),
            },
            None => Checked {
                decision,
                preauthorization: None,
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
        let e = engine_with(&a, &[&l1]);
        let tainted = CallContext { tainted: true };
        assert!(matches!(
            e.check(&a, &l1, tainted),
            Decision::NeedsConfirmation { .. }
        ));
    }

    fn pre(capability: &str, args: Vec<(&str, Matcher)>) -> Preauthorization {
        Preauthorization {
            id: "pre-1".into(),
            subject: Subject::User,
            capability: CapabilityName::new(capability),
            args: args
                .into_iter()
                .map(|(field, matcher)| ArgRule {
                    field: field.into(),
                    matcher,
                })
                .collect(),
            other_args: false,
            created_at_ms: 1_000,
            expires_at_ms: None,
        }
    }

    #[test]
    fn a_preauthorization_lifts_only_the_confirmation_of_the_calls_it_describes() {
        let a = AgentId::new("butler");
        let send = spec("mail.send", RiskLevel::External);
        let e = engine_with(&a, &[&send]);
        let rules = [pre(
            "mail.send",
            vec![
                ("to", Matcher::OneOf(vec![json!("me@example.com")])),
                ("body", Matcher::Any),
            ],
        )];
        let check = |args: Value, ctx: CallContext| {
            e.check_call(
                &a,
                &send,
                &args,
                ctx,
                Standing {
                    subject: &Subject::User,
                    preauthorizations: &rules,
                    now_ms: 2_000,
                },
            )
        };
        let clean = CallContext::default();
        let allowed = check(json!({"to": "me@example.com", "body": "hi"}), clean);
        assert_eq!(allowed.decision, Decision::Allow);
        assert_eq!(allowed.preauthorization.as_deref(), Some("pre-1"));
        // Every recipient must be allowed, not just the first.
        assert_eq!(
            check(json!({"to": ["me@example.com"], "body": "hi"}), clean).decision,
            Decision::Allow
        );
        for args in [
            json!({"to": "eve@example.com", "body": "hi"}),
            json!({"to": ["me@example.com", "eve@example.com"], "body": "hi"}),
            json!({"to": [], "body": "hi"}),
            // A required argument is missing.
            json!({"body": "hi"}),
            // Guards "an argument nobody looked at is not approved".
            json!({"to": "me@example.com", "body": "hi", "bcc": "eve@example.com"}),
            json!("not an object"),
        ] {
            let checked = check(args.clone(), clean);
            assert!(
                matches!(checked.decision, Decision::NeedsConfirmation { .. }),
                "{args}"
            );
            assert_eq!(checked.preauthorization, None);
        }
        // Guards "taint wins": the same call planned from outside content asks.
        assert!(matches!(
            check(
                json!({"to": "me@example.com", "body": "hi"}),
                CallContext { tainted: true }
            )
            .decision,
            Decision::NeedsConfirmation { .. }
        ));
    }

    #[test]
    fn a_preauthorization_never_covers_irreversible_ungranted_or_other_subjects_calls() {
        let a = AgentId::new("butler");
        let l2 = spec("l2", RiskLevel::External);
        let l3 = spec("l3", RiskLevel::Irreversible);
        let e = engine_with(&a, &[&l2, &l3]);
        let args = json!({"x": 1});
        let rules = [
            pre("l2", vec![("x", Matcher::Equals(json!(1)))]),
            pre("l3", vec![("x", Matcher::Equals(json!(1)))]),
        ];
        let check = |agent: &AgentId, spec: &CapabilitySpec, subject: Subject| {
            e.check_call(
                agent,
                spec,
                &args,
                CallContext::default(),
                Standing {
                    subject: &subject,
                    preauthorizations: &rules,
                    now_ms: 2_000,
                },
            )
            .decision
        };
        assert_eq!(check(&a, &l2, Subject::User), Decision::Allow);
        // Guards "L3 is never pre-authorized".
        assert!(matches!(
            check(&a, &l3, Subject::User),
            Decision::NeedsConfirmation {
                verify_identity: true,
                ..
            }
        ));
        // It lifts a confirmation, never a denial.
        assert!(matches!(
            check(&AgentId::new("stranger"), &l2, Subject::User),
            Decision::Deny { .. }
        ));
        // Guards "not across subjects".
        assert!(matches!(
            check(&a, &l2, Subject::Caller("other".into())),
            Decision::NeedsConfirmation { .. }
        ));
    }

    #[test]
    fn an_outside_caller_reads_freely_and_asks_for_everything_else_unless_preauthorized() {
        let a = AgentId::new("caller:helper");
        let l0 = spec("l0", RiskLevel::ReadOnly);
        let l1 = spec("l1", RiskLevel::Reversible);
        let l3 = spec("l3", RiskLevel::Irreversible);
        let e = engine_with(&a, &[&l0, &l1, &l3]);
        let caller = Subject::Caller("helper".into());
        let args = json!({"x": 1});
        let mut for_caller = pre("l1", vec![("x", Matcher::Equals(json!(1)))]);
        for_caller.subject = caller.clone();
        let for_user = pre("l1", vec![("x", Matcher::Equals(json!(1)))]);
        let check = |spec: &CapabilitySpec, subject: &Subject, rules: &[Preauthorization]| {
            e.check_call(
                &a,
                spec,
                &args,
                CallContext::default(),
                Standing {
                    subject,
                    preauthorizations: rules,
                    now_ms: 2_000,
                },
            )
        };
        assert_eq!(check(&l0, &caller, &[]).decision, Decision::Allow);
        // The user's own reversible call just runs; a caller's asks.
        assert_eq!(check(&l1, &Subject::User, &[]).decision, Decision::Allow);
        assert!(matches!(
            check(&l1, &caller, &[]).decision,
            Decision::NeedsConfirmation {
                verify_identity: false,
                ..
            }
        ));
        // Guards "not across subjects": the user's rule is not the caller's.
        assert!(matches!(
            check(&l1, &caller, std::slice::from_ref(&for_user)).decision,
            Decision::NeedsConfirmation { .. }
        ));
        let lifted = check(&l1, &caller, std::slice::from_ref(&for_caller));
        assert_eq!(lifted.decision, Decision::Allow);
        assert_eq!(lifted.preauthorization.as_deref(), Some("pre-1"));
        // Irreversible stays with the human and their identity.
        assert!(matches!(
            check(&l3, &caller, &[for_caller]).decision,
            Decision::NeedsConfirmation {
                verify_identity: true,
                ..
            }
        ));
    }

    #[test]
    fn a_preauthorization_expires_and_must_narrow_something() {
        let args = json!({"x": 1});
        let name = CapabilityName::new("l2");
        let mut timed = pre("l2", vec![]);
        timed.other_args = true;
        timed.expires_at_ms = Some(5_000);
        assert!(timed.validate().is_ok());
        assert!(timed.covers(&Subject::User, &name, &args, 4_999));
        assert!(!timed.covers(&Subject::User, &name, &args, 5_000));

        // Neither limited in time nor in arguments: not a pre-authorization.
        let mut unlimited = pre("l2", vec![("x", Matcher::Any)]);
        unlimited.other_args = true;
        assert!(unlimited.validate().is_err());
        for bad in [
            vec![("x", Matcher::OneOf(vec![]))],
            vec![("x", Matcher::Prefix(String::new()))],
            vec![("p", Matcher::Under("/a/../b".into()))],
            vec![
                ("x", Matcher::Equals(json!(1))),
                ("x", Matcher::Equals(json!(2))),
            ],
        ] {
            assert!(pre("l2", bad).validate().is_err());
        }
    }

    #[test]
    fn a_path_rule_cannot_be_climbed_out_of_and_a_prefix_is_only_text() {
        let under = Matcher::Under("/home/a/docs".into());
        for inside in ["/home/a/docs", "/home/a/docs/x.txt", "/home//a/docs/sub/y"] {
            assert!(under.accepts(&json!(inside)), "{inside}");
        }
        for outside in [
            "/home/a/docs-evil/x",
            "/home/a/docs/../secret",
            "/home/a/docs/./x",
            "home/a/docs/x",
            "/home/a",
            "",
        ] {
            assert!(!under.accepts(&json!(outside)), "{outside}");
        }
        assert!(!under.accepts(&json!(7)));
        assert!(under.accepts(&json!(["/home/a/docs/x", "/home/a/docs/y"])));
        assert!(!under.accepts(&json!(["/home/a/docs/x", "/etc/passwd"])));
        // Text only: this is why paths get their own rule.
        assert!(Matcher::Prefix("/home/a/docs".into()).accepts(&json!("/home/a/docs-evil/x")));
        assert!(Matcher::Equals(json!({"a": 1})).accepts(&json!({"a": 1})));
    }

    #[test]
    fn a_preauthorization_round_trips_and_rejects_unknown_fields() {
        let mut rule = pre(
            "mail.send",
            vec![
                ("to", Matcher::OneOf(vec![json!("me@example.com")])),
                ("path", Matcher::Under("/tmp/out".into())),
                ("body", Matcher::Any),
            ],
        );
        rule.expires_at_ms = Some(9_000);
        let text = serde_json::to_value(&rule).unwrap();
        assert_eq!(
            text["args"][0],
            json!({"field": "to", "match": {"one_of": ["me@example.com"]}})
        );
        assert_eq!(text["args"][2], json!({"field": "body", "match": "any"}));
        assert_eq!(text["subject"], json!({"kind": "user"}));
        assert_eq!(
            serde_json::from_value::<Preauthorization>(text.clone()).unwrap(),
            rule
        );
        let mut extra = text;
        extra["surprise"] = json!(true);
        assert!(serde_json::from_value::<Preauthorization>(extra).is_err());
    }

    #[test]
    fn set_grants_replaces_previous_grants() {
        let mut e = PolicyEngine::new();
        let a = AgentId::new("sub");
        let l0 = spec("l0", RiskLevel::ReadOnly);
        let other = spec("other", RiskLevel::ReadOnly);
        e.grant(&a, &other.name);
        e.set_grants(&a, [l0.name.clone()]);
        assert_eq!(e.check(&a, &l0, CallContext::default()), Decision::Allow);
        assert!(!e.is_granted(&a, &other.name));
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
