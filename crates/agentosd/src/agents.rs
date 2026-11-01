//! Agents the butler can delegate to.
//!
//! An agent is described by a manifest: who it is, what it is good at (the
//! butler reads this to pick one), which capabilities it may ask for, and the
//! highest risk level it may touch. The butler offers each agent to its
//! planner as a tool named `agent.<id>`; calling it hands the agent a task.
//!
//! What a delegate may actually use is always narrowed, never widened:
//! capabilities matching its manifest, AND held by the butler, AND at or below
//! its risk ceiling. Delegates cannot delegate further, and never talk to the
//! human directly: confirmations and results go back through the butler.

use std::collections::BTreeMap;
use std::path::Path;

use agentos_core::{AgentId, CapabilityName, CapabilitySpec, RiskLevel};
use serde::Deserialize;
use serde_json::json;

/// Prefix of the capability names that stand for delegation to an agent.
pub const DELEGATE_PREFIX: &str = "agent.";

pub const DEFAULT_MAX_ROUNDS: u32 = 8;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AgentManifest {
    pub id: String,
    pub name: String,
    /// One sentence on what the agent is good at.
    pub description: String,
    /// Capability names or prefix patterns ending in `*` (`notes.*`).
    pub capabilities: Vec<String>,
    /// Highest risk level the agent may touch (`L0`..`L3`, or
    /// `read_only` / `reversible` / `external` / `irreversible`).
    #[serde(default = "default_max_risk", deserialize_with = "risk_from_str")]
    pub max_risk: RiskLevel,
    /// Extra instructions appended to the sub-agent system prompt.
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default = "default_rounds")]
    pub max_rounds: u32,
}

fn default_max_risk() -> RiskLevel {
    RiskLevel::Reversible
}

fn default_rounds() -> u32 {
    DEFAULT_MAX_ROUNDS
}

pub fn parse_risk(s: &str) -> Option<RiskLevel> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "l0" | "read_only" => RiskLevel::ReadOnly,
        "l1" | "reversible" => RiskLevel::Reversible,
        "l2" | "external" => RiskLevel::External,
        "l3" | "irreversible" => RiskLevel::Irreversible,
        _ => return None,
    })
}

fn risk_from_str<'de, D: serde::Deserializer<'de>>(d: D) -> Result<RiskLevel, D::Error> {
    let s = String::deserialize(d)?;
    parse_risk(&s).ok_or_else(|| serde::de::Error::custom(format!("unknown risk level {s:?}")))
}

impl AgentManifest {
    pub fn agent_id(&self) -> AgentId {
        AgentId::new(&self.id)
    }

    /// The capability that stands for "delegate to this agent".
    pub fn delegate_name(&self) -> CapabilityName {
        CapabilityName::new(format!("{DELEGATE_PREFIX}{}", self.id))
    }

    /// The tool the butler's planner sees for this agent. Delegation itself is
    /// read-only: every call the agent then makes is checked on its own.
    pub fn delegate_spec(&self) -> CapabilitySpec {
        CapabilitySpec {
            name: self.delegate_name(),
            description: format!(
                "交给「{}」去做：{}。它只能使用自己被授权的能力，做完会把结果交还给你。",
                self.name, self.description
            ),
            risk: RiskLevel::ReadOnly,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "要它完成的任务：目标、已知信息、需要交回的结果"
                    }
                },
                "required": ["task"]
            }),
        }
    }

    /// Whether the manifest asks for `cap`.
    pub fn wants(&self, cap: &CapabilityName) -> bool {
        let name = cap.as_str();
        if name.starts_with(DELEGATE_PREFIX) {
            return false; // no nested delegation
        }
        self.capabilities.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == p,
        })
    }

    /// What the agent may use, given what the delegating agent holds.
    pub fn effective(
        &self,
        available: &[CapabilitySpec],
        delegator_holds: impl Fn(&CapabilityName) -> bool,
    ) -> Vec<CapabilitySpec> {
        available
            .iter()
            .filter(|s| self.wants(&s.name) && s.risk <= self.max_risk && delegator_holds(&s.name))
            .cloned()
            .collect()
    }

    fn validate(&self) -> Result<(), String> {
        let ok_id = !self.id.is_empty()
            && self.id.len() <= 32
            && self
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if !ok_id {
            return Err(format!(
                "Agent id {:?} 只能用小写字母、数字、- 和 _，最长 32 个字符",
                self.id
            ));
        }
        if self.id == "butler" {
            return Err("Agent id 不能是 butler".into());
        }
        if self.name.trim().is_empty() || self.description.trim().is_empty() {
            return Err(format!("Agent {} 缺少 name 或 description", self.id));
        }
        if self.max_rounds == 0 || self.max_rounds > 32 {
            return Err(format!(
                "Agent {} 的 max_rounds 必须在 1 到 32 之间",
                self.id
            ));
        }
        Ok(())
    }
}

/// The agents shipped with the system.
pub fn builtin() -> Vec<AgentManifest> {
    vec![
        AgentManifest {
            id: "notes".into(),
            name: "笔记 Agent".into(),
            description: "管理用户的笔记：记录、查找、整理和清理".into(),
            capabilities: vec!["notes.*".into(), "clock.now".into()],
            max_risk: RiskLevel::Irreversible,
            instructions: Some(
                "整理笔记时先列出现有笔记，再决定怎么做。清空笔记不可撤销，只在任务明确要求时才做。"
                    .into(),
            ),
            max_rounds: DEFAULT_MAX_ROUNDS,
        },
        AgentManifest {
            id: "craftsman".into(),
            name: "能力工匠 Agent".into(),
            description: "为系统编写、改进和维护新能力插件（自我进化），也能查看、停用或回滚已安装的插件".into(),
            capabilities: vec!["evolve.*".into()],
            max_risk: RiskLevel::Irreversible,
            instructions: Some(
                "要写新能力时调用 evolve__propose，need 写清楚输入、输出和边界情况。\
不要自己调用 evolve__install，系统会在用户审阅草案后发起安装确认。"
                    .into(),
            ),
            max_rounds: DEFAULT_MAX_ROUNDS,
        },
    ]
}

/// The agents the butler knows, by id.
#[derive(Debug, Clone, Default)]
pub struct AgentDirectory {
    agents: BTreeMap<String, AgentManifest>,
}

impl AgentDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_builtin() -> Self {
        let mut d = Self::new();
        for m in builtin() {
            d.add(m).expect("built-in agents are valid");
        }
        d
    }

    /// Add or replace an agent.
    pub fn add(&mut self, m: AgentManifest) -> Result<(), String> {
        m.validate()?;
        self.agents.insert(m.id.clone(), m);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&AgentManifest> {
        self.agents.get(id)
    }

    /// The agent a delegation capability stands for.
    pub fn by_capability(&self, cap: &CapabilityName) -> Option<&AgentManifest> {
        self.get(cap.as_str().strip_prefix(DELEGATE_PREFIX)?)
    }

    pub fn all(&self) -> impl Iterator<Item = &AgentManifest> {
        self.agents.values()
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Load `*.json` manifests from `dir` (missing dir = nothing). A file with
    /// the id of a built-in agent replaces it. Returns one note per problem.
    pub fn load_dir(&mut self, dir: &Path) -> Vec<String> {
        let mut notes = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return notes;
        };
        let mut paths: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        paths.sort();
        for p in paths {
            let parsed = std::fs::read_to_string(&p)
                .map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str::<AgentManifest>(&t).map_err(|e| e.to_string()))
                .and_then(|m| self.add(m));
            if let Err(e) = parsed {
                notes.push(format!("跳过 Agent 清单 {}：{e}", p.display()));
            }
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, risk: RiskLevel) -> CapabilitySpec {
        CapabilitySpec {
            name: CapabilityName::new(name),
            description: String::new(),
            risk,
            input_schema: json!({}),
        }
    }

    #[test]
    fn effective_is_manifest_and_delegator_and_risk_ceiling() {
        let m = AgentManifest {
            id: "t".into(),
            name: "T".into(),
            description: "d".into(),
            capabilities: vec!["notes.*".into(), "clock.now".into(), "agent.*".into()],
            max_risk: RiskLevel::Reversible,
            instructions: None,
            max_rounds: 4,
        };
        let all = vec![
            spec("notes.append", RiskLevel::Reversible),
            spec("notes.clear", RiskLevel::Irreversible),
            spec("notes.list", RiskLevel::ReadOnly),
            spec("clock.now", RiskLevel::ReadOnly),
            spec("evolve.list", RiskLevel::ReadOnly),
            spec("agent.other", RiskLevel::ReadOnly),
        ];
        let names: Vec<String> = m
            .effective(&all, |c| c.as_str() != "notes.list")
            .into_iter()
            .map(|s| s.name.to_string())
            .collect();
        // notes.clear: above the ceiling; notes.list: the butler lacks it;
        // evolve.list: not in the manifest; agent.other: no nested delegation.
        assert_eq!(names, vec!["notes.append", "clock.now"]);
    }

    #[test]
    fn manifest_json_parses_with_defaults() {
        let m: AgentManifest = serde_json::from_str(
            r#"{"id":"files","name":"文件 Agent","description":"整理文件","capabilities":["fs.*"],"max_risk":"L2"}"#,
        )
        .unwrap();
        assert_eq!(m.max_risk, RiskLevel::External);
        assert_eq!(m.max_rounds, DEFAULT_MAX_ROUNDS);
        assert_eq!(m.delegate_name().as_str(), "agent.files");
    }

    #[test]
    fn bad_ids_are_rejected() {
        let mut d = AgentDirectory::new();
        let mut m = builtin().remove(0);
        m.id = "Bad Id".into();
        assert!(d.add(m.clone()).is_err());
        m.id = "butler".into();
        assert!(d.add(m).is_err());
    }

    #[test]
    fn load_dir_adds_and_reports() {
        let dir = std::env::temp_dir().join(format!("agentos-agents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.json"),
            r#"{"id":"files","name":"文件 Agent","description":"整理文件","capabilities":["fs.*"]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("b.json"), "{not json").unwrap();
        let mut d = AgentDirectory::with_builtin();
        let notes = d.load_dir(&dir);
        assert_eq!(notes.len(), 1);
        assert!(d.get("files").is_some());
        assert!(d
            .by_capability(&CapabilityName::new("agent.notes"))
            .is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
