//! Self-evolution: AgentOS writes, tests and installs new capabilities for
//! itself when the human asks for something it cannot do yet.
//!
//! The loop, and who is in charge of each step:
//!
//! 1. **Propose** (`evolve.propose`, L1): a [`PluginGenerator`] (an LLM) writes
//!    a plugin for the stated need. The system validates it, runs its
//!    self-tests in the sandbox and feeds failures back for another attempt.
//!    Nothing is installed; the draft is held in memory.
//! 2. **Review**: the human sees the summary, permissions, effective risk,
//!    test results and the code (or a diff against the current version).
//! 3. **Install** (`evolve.install`, L3): runs only after explicit
//!    confirmation. Writes an immutable new version and activates it; the
//!    butler grants itself the new capability on its next step.
//! 4. **Undo** (`evolve.rollback` / `evolve.disable`, L1): every earlier
//!    version stays on disk.
//!
//! Every step goes through the policy engine and lands in the audit log,
//! including the SHA-256 of what was installed.

use std::sync::Arc;

use agentos_core::{Capability, CapabilityError, CapabilityName, CapabilitySpec, RiskLevel};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::llm::TextModel;
use crate::plugin::{effective_risk, risk_label, PluginManifest, RUNTIMES};
use crate::plugin_store::{valid_name, PluginDraft, PluginManager, TestOutcome};
use crate::registry::CapabilityRegistry;

/// What the generator is asked to do.
#[derive(Debug, Clone)]
pub struct GenerationRequest {
    pub need: String,
    /// Capabilities that already exist (so it does not duplicate them).
    pub available: Vec<CapabilitySpec>,
    /// The plugin being improved, if this is an upgrade.
    pub existing: Option<PluginDraft>,
    /// The previous attempt and why it was rejected.
    pub retry: Option<(PluginDraft, String)>,
}

#[async_trait]
pub trait PluginGenerator: Send + Sync {
    async fn generate(&self, req: &GenerationRequest) -> Result<PluginDraft, String>;
}

/// Used when no model is configured.
pub struct NoGenerator;

#[async_trait]
impl PluginGenerator for NoGenerator {
    async fn generate(&self, _req: &GenerationRequest) -> Result<PluginDraft, String> {
        Err("自我进化需要大模型：请设置 ANTHROPIC_API_KEY 或 OpenAI 兼容接口".into())
    }
}

pub const GENERATOR_PROMPT: &str = r#"你是 AgentOS 的能力工程师。AgentOS 是一个面向 AI agent 的操作系统，用户提出了一个系统现在做不到的需求，你要为它编写一个“能力插件”。

插件规则：
- 插件是一个独立的 python3 脚本（只能用标准库，不能 pip 安装）。也可以用 POSIX sh。
- 从 stdin 读取一个 JSON 对象作为参数（符合你给出的 input_schema）。
- 向 stdout 输出一个 JSON 对象，必须包含字符串字段 "message"：给用户看的结果，使用用户的语言，简洁。可以附带其他结构化字段。
- 失败时向 stderr 写原因并以非零状态退出。
- 运行在沙箱里：系统目录只读，没有用户主目录；只有 permissions.network 为 true 才能联网；只有 permissions.storage 为 true 才有一个可写的持久目录，路径在环境变量 AGENTOS_DATA；/tmp 可写但不持久。每次调用最多 10 秒。
- 只申请需求真正需要的权限。不要读取或修改沙箱外的任何东西，不要调用其他程序去绕过沙箱。

风险等级 risk（按最坏情况选）：
- "read_only"：只读、只在本机。
- "reversible"：在本机产生可撤销的变化（例如写自己的数据目录）。
- "external"：数据会离开本机、联系外部服务或产生费用。
- "irreversible"：不可撤销或涉及安全。

名字 name 用 "命名空间.动作" 形式，小写字母、数字和下划线，例如 "weather.today"、"unit.convert"。不要使用命名空间 evolve、system、policy、audit，不要与已有能力重名（升级已有插件时除外，此时必须保持原名）。

自测 tests：给出 1 到 5 个用例，每个用例包含 args（参数对象）和可选的 expect_contains（输出的 message 必须包含的文字）。用例必须在沙箱和所申请的权限下可以通过，结果要确定（不要断言当前时间、随机数或网络上会变化的具体数值）。

只输出一个 JSON 对象，不要任何其他文字或代码块标记，字段如下：
{
  "name": "...",
  "description": "一句话说明这个能力做什么（会作为工具描述给管家 Agent 看）",
  "risk": "read_only|reversible|external|irreversible",
  "input_schema": {"type": "object", "properties": {...}, "required": [...]},
  "permissions": {"network": false, "storage": false},
  "runtime": "python3",
  "summary": "用用户的语言向用户说明代码做了什么、申请了哪些权限以及原因",
  "tests": [{"args": {...}, "expect_contains": "..."}],
  "source": "完整的脚本源码"
}"#;

/// Builds the user message for the generator.
pub fn generation_prompt(req: &GenerationRequest) -> String {
    let mut s = format!("用户的需求：{}\n\n系统已有的能力：\n", req.need);
    for c in &req.available {
        s.push_str(&format!("- {}：{}\n", c.name, c.description));
    }
    if let Some(e) = &req.existing {
        s.push_str(&format!(
            "\n这是对已安装插件 {} 的改进。当前清单：\n{}\n当前源码：\n{}\n",
            e.manifest.name,
            serde_json::to_string_pretty(&strip_source_fields(&e.manifest)).unwrap_or_default(),
            e.source
        ));
    }
    if let Some((prev, why)) = &req.retry {
        s.push_str(&format!(
            "\n你上一次给出的插件没有通过检查，请修正后重新输出完整 JSON。\n问题：\n{why}\n上一次的源码：\n{}\n",
            prev.source
        ));
    }
    s
}

fn strip_source_fields(m: &PluginManifest) -> Value {
    let mut v = serde_json::to_value(m).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.remove("need");
    }
    v
}

/// Parse the generator's reply: one JSON object, possibly wrapped in prose or
/// a code fence.
pub fn parse_draft(text: &str, need: &str) -> Result<PluginDraft, String> {
    let start = text.find('{').ok_or("模型没有返回 JSON")?;
    let end = text.rfind('}').ok_or("模型没有返回 JSON")?;
    let mut v: Value = serde_json::from_str(&text[start..=end])
        .map_err(|e| format!("模型返回的 JSON 无法解析：{e}"))?;
    let source = v
        .get("source")
        .and_then(Value::as_str)
        .ok_or("缺少 source 字段")?
        .to_string();
    if let Some(o) = v.as_object_mut() {
        o.remove("source");
        o.insert("need".into(), Value::String(need.to_string()));
    }
    let manifest: PluginManifest =
        serde_json::from_value(v).map_err(|e| format!("插件清单不完整：{e}"))?;
    Ok(PluginDraft { manifest, source })
}

pub struct LlmGenerator {
    model: Arc<dyn TextModel>,
}

impl LlmGenerator {
    pub fn new(model: Arc<dyn TextModel>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl PluginGenerator for LlmGenerator {
    async fn generate(&self, req: &GenerationRequest) -> Result<PluginDraft, String> {
        let text = self
            .model
            .complete(GENERATOR_PROMPT, &generation_prompt(req))
            .await?;
        parse_draft(&text, &req.need)
    }
}

/// Static checks before any code runs. Returns every problem found.
pub fn validate(draft: &PluginDraft, pm: &PluginManager, target: Option<&str>) -> Vec<String> {
    let m = &draft.manifest;
    let name = m.name.as_str();
    let mut p = Vec::new();
    if !valid_name(name) {
        p.push(format!(
            "名字 {name} 不合法，应为 命名空间.动作（小写字母、数字、下划线）"
        ));
    } else if pm.is_reserved(name) {
        p.push(format!("名字 {name} 是系统保留的，换一个"));
    }
    if let Some(t) = target {
        if name != t {
            p.push(format!("这是对 {t} 的升级，名字必须保持为 {t}"));
        }
    }
    if m.entry_file().is_none() {
        let rts: Vec<&str> = RUNTIMES.iter().map(|(r, _)| *r).collect();
        p.push(format!(
            "运行时 {} 不支持，可选：{}",
            m.runtime,
            rts.join("、")
        ));
    }
    if m.description.trim().is_empty() || m.description.chars().count() > 200 {
        p.push("description 必须是 1 到 200 字的一句话".into());
    }
    if m.input_schema.get("type").and_then(Value::as_str) != Some("object") {
        p.push("input_schema 必须是 type 为 object 的 JSON Schema".into());
    }
    if draft.source.trim().is_empty() {
        p.push("source 是空的".into());
    }
    if draft.source.len() > 64 * 1024 {
        p.push("source 超过 64 KiB".into());
    }
    if m.tests.is_empty() || m.tests.len() > 8 {
        p.push("需要 1 到 5 个自测用例".into());
    }
    if m.tests.iter().any(|t| !t.args.is_object()) {
        p.push("每个自测用例的 args 必须是 JSON 对象".into());
    }
    p
}

fn failures(outcomes: &[TestOutcome]) -> Option<String> {
    let bad: Vec<String> = outcomes
        .iter()
        .filter(|o| !o.passed)
        .map(|o| format!("- 参数 {}：{}", o.args, o.detail))
        .collect();
    (!bad.is_empty()).then(|| format!("自测失败：\n{}", bad.join("\n")))
}

/// A minimal line diff (LCS), enough to review a plugin upgrade.
pub fn line_diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a.len() * b.len() > 4_000_000 {
        return b.iter().map(|l| format!("+ {l}\n")).collect();
    }
    let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, String::new());
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            out.push_str(&format!("  {}\n", a[i]));
            i += 1;
            j += 1;
        } else if i < a.len() && (j == b.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            out.push_str(&format!("- {}\n", a[i]));
            i += 1;
        } else {
            out.push_str(&format!("+ {}\n", b[j]));
            j += 1;
        }
    }
    out
}

const MAX_SHOWN_LINES: usize = 200;

fn clip(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= MAX_SHOWN_LINES {
        return text.trim_end().to_string();
    }
    format!(
        "{}\n…（共 {} 行，只显示前 {MAX_SHOWN_LINES} 行）",
        lines[..MAX_SHOWN_LINES].join("\n"),
        lines.len()
    )
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "是"
    } else {
        "否"
    }
}

fn args_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn spec(name: &str, description: &str, risk: RiskLevel, input_schema: Value) -> CapabilitySpec {
    CapabilitySpec {
        name: CapabilityName::new(name),
        description: description.into(),
        risk,
        input_schema,
    }
}

fn io_err(e: std::io::Error) -> CapabilityError {
    CapabilityError::Failed(e.to_string())
}

pub struct EvolvePropose {
    spec: CapabilitySpec,
    pm: Arc<PluginManager>,
    generator: Arc<dyn PluginGenerator>,
    /// What already exists, refreshed by the butler on each call.
    available: Arc<dyn Fn() -> Vec<CapabilitySpec> + Send + Sync>,
    max_attempts: usize,
}

#[async_trait]
impl Capability for EvolvePropose {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        let need = args_str(&args, "need")
            .ok_or_else(|| CapabilityError::InvalidArgs("`need` 必须是非空字符串".into()))?
            .to_string();
        let target = args_str(&args, "target").map(str::to_string);
        let existing = match &target {
            Some(t) => Some(
                self.pm
                    .active_draft(t)
                    .ok_or_else(|| CapabilityError::InvalidArgs(format!("没有启用中的插件 {t}")))?
                    .1,
            ),
            None => None,
        };
        let mut req = GenerationRequest {
            need,
            available: (self.available)(),
            existing,
            retry: None,
        };

        let mut last_problem = String::new();
        for attempt in 1..=self.max_attempts {
            let draft = self
                .generator
                .generate(&req)
                .await
                .map_err(CapabilityError::Failed)?;
            let problems = validate(&draft, &self.pm, target.as_deref());
            if !problems.is_empty() {
                last_problem = format!("清单检查未通过：\n- {}", problems.join("\n- "));
                req.retry = Some((draft, last_problem.clone()));
                continue;
            }
            let outcomes = self.pm.test_draft(&draft).await.map_err(io_err)?;
            if let Some(why) = outcomes.as_deref().and_then(failures) {
                last_problem = why;
                req.retry = Some((draft, last_problem.clone()));
                continue;
            }
            return Ok(self.present(draft, outcomes, attempt));
        }
        Err(CapabilityError::Failed(format!(
            "尝试了 {} 次仍没有写出合格的插件。最后一次的问题：{last_problem}",
            self.max_attempts
        )))
    }
}

impl EvolvePropose {
    fn present(
        &self,
        draft: PluginDraft,
        outcomes: Option<Vec<TestOutcome>>,
        attempts: usize,
    ) -> Value {
        let m = &draft.manifest;
        let name = m.name.as_str().to_string();
        let sandbox = self.pm.sandbox();
        let risk = effective_risk(m, sandbox);
        let version = self.pm.next_version(&name);
        let current = self.pm.active_draft(&name);
        let entry = m.entry_file().unwrap_or("main");

        let tests = match &outcomes {
            Some(o) => format!(
                "{}/{} 通过（{}，第 {attempts} 次生成）",
                o.iter().filter(|t| t.passed).count(),
                o.len(),
                sandbox.label()
            ),
            None => {
                "未运行：本机没有可用的沙箱，安装前请自己审阅代码（建议安装 bubblewrap）".into()
            }
        };
        let code = match &current {
            Some((v, old)) => format!(
                "与当前 v{v} 的差异（{entry}）：\n{}",
                clip(&line_diff(&old.source, &draft.source))
            ),
            None => format!(
                "代码（{entry}，{} 行）：\n{}",
                draft.source.lines().count(),
                clip(&draft.source)
            ),
        };
        let action = if current.is_some() {
            "升级"
        } else {
            "新能力"
        };
        let message = format!(
            "{action}草案：{name}（将安装为 v{version}）\n\
             用途：{}\n\
             说明：{}\n\
             权限：联网 {} · 持久存储 {}\n\
             风险：声明 {} → 生效 {}\n\
             自测：{tests}\n\
             {code}",
            m.description,
            if m.summary.is_empty() {
                "（无）"
            } else {
                &m.summary
            },
            yes_no(m.permissions.network),
            yes_no(m.permissions.storage),
            risk_label(m.risk),
            risk_label(risk),
        );
        let digest = draft.digest();
        let id = self.pm.stage(draft);
        json!({
            "draft": id,
            "name": name,
            "version": version,
            "effective_risk": risk,
            "digest": digest,
            "tests": outcomes,
            "message": message,
            "follow_up": {"capability": "evolve.install", "args": {"draft": id}},
        })
    }
}

pub struct EvolveInstall {
    spec: CapabilitySpec,
    pm: Arc<PluginManager>,
}

#[async_trait]
impl Capability for EvolveInstall {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        let id = args_str(&args, "draft")
            .ok_or_else(|| CapabilityError::InvalidArgs("`draft` 必须是草案编号".into()))?;
        let r = self.pm.install(id).map_err(io_err)?;
        let undo = match r.previous {
            Some(p) => format!("说“回滚 {}”可恢复到 v{p}", r.name),
            None => format!("说“停用 {}”可关闭它", r.name),
        };
        Ok(json!({
            "name": r.name,
            "version": r.version,
            "previous": r.previous,
            "digest": r.digest,
            "message": format!("已安装 {} v{}，现在就可以用了。{undo}。", r.name, r.version),
        }))
    }
}

pub struct EvolveList {
    spec: CapabilitySpec,
    pm: Arc<PluginManager>,
}

#[async_trait]
impl Capability for EvolveList {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, _args: Value) -> Result<Value, CapabilityError> {
        let list = self.pm.list();
        let message = if list.is_empty() {
            "还没有自己生成的能力。告诉我你需要什么，我可以学。".to_string()
        } else {
            list.iter()
                .map(|p| {
                    let state = p
                        .current
                        .map_or("已停用".to_string(), |v| format!("v{v} 启用中"));
                    let vs: Vec<String> = p.versions.iter().map(|v| format!("v{v}")).collect();
                    format!(
                        "- {}：{}（{state}；版本 {}；{}）",
                        p.name,
                        p.description,
                        vs.join("、"),
                        risk_label(p.risk)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(json!({"plugins": list, "message": message}))
    }
}

pub struct EvolveRollback {
    spec: CapabilitySpec,
    pm: Arc<PluginManager>,
}

#[async_trait]
impl Capability for EvolveRollback {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        let name = args_str(&args, "name")
            .ok_or_else(|| CapabilityError::InvalidArgs("`name` 必须是插件名".into()))?;
        let to = args
            .get("version")
            .and_then(Value::as_u64)
            .map(|v| v as u32);
        let (from, to) = self.pm.rollback(name, to).map_err(io_err)?;
        let from = from.map_or("停用状态".to_string(), |v| format!("v{v}"));
        Ok(json!({
            "name": name,
            "version": to,
            "message": format!("{name} 已从 {from} 切换到 v{to}"),
        }))
    }
}

pub struct EvolveDisable {
    spec: CapabilitySpec,
    pm: Arc<PluginManager>,
}

#[async_trait]
impl Capability for EvolveDisable {
    fn spec(&self) -> &CapabilitySpec {
        &self.spec
    }

    async fn invoke(&self, args: Value) -> Result<Value, CapabilityError> {
        let name = args_str(&args, "name")
            .ok_or_else(|| CapabilityError::InvalidArgs("`name` 必须是插件名".into()))?;
        let v = self.pm.disable(name).map_err(io_err)?;
        Ok(json!({
            "name": name,
            "message": format!(
                "已停用 {name}（v{}），版本和数据都保留着，说“回滚 {name}”或“恢复 {name}”可重新启用",
                v.unwrap_or(0)
            ),
        }))
    }
}

/// Register the `evolve.*` capabilities. Call after the built-ins so that
/// their names are reserved against plugins.
pub fn register_all(
    reg: &mut CapabilityRegistry,
    pm: Arc<PluginManager>,
    generator: Arc<dyn PluginGenerator>,
) {
    pm.reserve(reg.specs().into_iter().map(|s| s.name));
    let name_schema = json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "description": "插件名，例如 weather.today"},
            "version": {"type": "integer", "description": "要切换到的版本号；不填则回到上一个版本"}
        },
        "required": ["name"]
    });
    let snapshot = reg.clone();
    let pm_for_list = pm.clone();
    reg.register(Arc::new(EvolvePropose {
        spec: spec(
            "evolve.propose",
            "为系统编写一个新能力（插件）或改进已安装的插件：生成代码、在沙箱中自测，然后交给用户审阅。现有能力做不到用户的要求，或用户要求你学会/改进某项能力时使用。",
            RiskLevel::Reversible,
            json!({
                "type": "object",
                "properties": {
                    "need": {"type": "string", "description": "用户需求的完整描述：要做什么、需要哪些输入、期望什么输出"},
                    "target": {"type": "string", "description": "要改进的已安装插件名；新能力不填"}
                },
                "required": ["need"]
            }),
        ),
        pm: pm.clone(),
        generator,
        available: Arc::new(move || {
            let mut specs = snapshot.specs();
            specs.extend(pm_for_list.active().iter().map(|c| c.spec().clone()));
            specs
        }),
        max_attempts: 3,
    }));
    reg.register(Arc::new(EvolveInstall {
        spec: spec(
            "evolve.install",
            "安装刚才展示给用户审阅的插件草案（由系统在草案生成后自动发起）",
            RiskLevel::Irreversible,
            json!({"type": "object", "properties": {"draft": {"type": "string"}}, "required": ["draft"]}),
        ),
        pm: pm.clone(),
    }));
    reg.register(Arc::new(EvolveList {
        spec: spec(
            "evolve.list",
            "列出系统自己生成的能力（插件）及其版本",
            RiskLevel::ReadOnly,
            json!({"type": "object", "properties": {}}),
        ),
        pm: pm.clone(),
    }));
    reg.register(Arc::new(EvolveRollback {
        spec: spec(
            "evolve.rollback",
            "把插件切换回之前的版本，也可用来重新启用已停用的插件",
            RiskLevel::Reversible,
            name_schema,
        ),
        pm: pm.clone(),
    }));
    reg.register(Arc::new(EvolveDisable {
        spec: spec(
            "evolve.disable",
            "停用一个插件（保留所有版本和数据，可恢复）",
            RiskLevel::Reversible,
            json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}),
        ),
        pm,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_json_and_keeps_need() {
        let text = "好的：\n```json\n{\"name\":\"unit.convert\",\"description\":\"单位换算\",\"risk\":\"read_only\",\
                    \"input_schema\":{\"type\":\"object\"},\"runtime\":\"python3\",\"tests\":[{\"args\":{}}],\
                    \"source\":\"print('{}')\"}\n```";
        let d = parse_draft(text, "换算单位").unwrap();
        assert_eq!(d.manifest.name.as_str(), "unit.convert");
        assert_eq!(d.manifest.need, "换算单位");
        assert_eq!(d.source, "print('{}')");
        assert!(!d.manifest.permissions.network);
    }

    #[test]
    fn rejects_reply_without_source() {
        assert!(parse_draft("{\"name\":\"a.b\"}", "x").is_err());
        assert!(parse_draft("抱歉", "x").is_err());
    }

    #[test]
    fn diff_marks_changes() {
        let d = line_diff("a\nb\nc\n", "a\nB\nc\nd\n");
        assert_eq!(d, "  a\n- b\n+ B\n  c\n+ d\n");
    }
}
