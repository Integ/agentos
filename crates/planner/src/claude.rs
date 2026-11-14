//! Planner backed by the Claude Messages API.
//!
//! Each planning round is one `POST /v1/messages` call. Every capability the
//! agent may use is offered as a tool; the `tool_use` blocks in the response
//! become plan steps (the planner never executes anything itself — the agent
//! runs each step through the policy engine). Text blocks become the reply.
//! Earlier rounds are replayed as `tool_use` / `tool_result` pairs so the model
//! can act on what its calls returned.
//!
//! Rust has no official Anthropic SDK, so this speaks raw HTTP.

use std::time::Duration;

use agentos_core::{CapabilitySpec, Intent};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::llm::{send_with_retry, Retry, TextModel};
use crate::{capability_name, tool_name, Conversation, Plan, Planner, Step, Usage, SYSTEM_PROMPT};

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

#[derive(Debug, Clone)]
pub struct ClaudeConfig {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
    pub effort: String,
    pub max_tokens: u32,
}

impl ClaudeConfig {
    /// Build from `ANTHROPIC_API_KEY`, `AGENTOS_MODEL`, `ANTHROPIC_BASE_URL`.
    /// Returns `None` when no API key is set.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())?;
        Some(Self {
            api_key,
            model: std::env::var("AGENTOS_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into()),
            base_url: std::env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_BASE_URL.into()),
            effort: "medium".into(),
            max_tokens: 16_000,
        })
    }
}

/// Request body for one intent (first round, butler prompt).
pub fn build_request(cfg: &ClaudeConfig, intent: &Intent, available: &[CapabilitySpec]) -> Value {
    build_turn_request(
        cfg,
        &Conversation {
            system: SYSTEM_PROMPT,
            intent,
            history: &[],
            available,
        },
    )
}

/// Request body for the next round of a conversation.
pub fn build_turn_request(cfg: &ClaudeConfig, conv: &Conversation<'_>) -> Value {
    build(cfg, conv, true).0
}

/// Identifies a system prompt and tool list. Kept with each recorded turn so
/// the turn is replayed only while both are unchanged. Hashed in memory
/// only: turns are never persisted, so the hash need not be stable across
/// builds.
fn prefix_of(system: &str, tools: &[Value]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    system.hash(&mut hasher);
    serde_json::to_string(tools)
        .unwrap_or_default()
        .hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The first round from which recorded turns are sent back verbatim: the
/// trailing run of rounds produced under the current prefix. Earlier rounds
/// go back rebuilt without their reasoning; dropping a leading run of
/// reasoning blocks is allowed, editing what lies before a kept one is not.
fn replay_from(history: &[crate::Round], prefix: &str) -> usize {
    history
        .iter()
        .rposition(|round| round.turn.as_ref().is_none_or(|turn| turn.prefix != prefix))
        .map_or(0, |last_stale| last_stale + 1)
}

/// The body, and the prefix it was built under. With `replay` off, every
/// round is rebuilt without reasoning, which the service always accepts.
fn build(cfg: &ClaudeConfig, conv: &Conversation<'_>, replay: bool) -> (Value, String) {
    let tools: Vec<Value> = conv
        .available
        .iter()
        .map(|s| {
            json!({
                "name": tool_name(&s.name),
                "description": format!("{}（风险等级：{:?}）", s.description, s.risk),
                "input_schema": s.input_schema,
            })
        })
        .collect();
    let mut user = conv.intent.text.clone();
    if let Some(speaker) = &conv.intent.speaker {
        user = format!("[{speaker}] {user}");
    }
    let mut messages = vec![json!({
        "role": "user",
        "content": [{ "type": "text", "text": user }],
    })];
    let prefix = prefix_of(conv.system, &tools);
    let verbatim_from = if replay {
        replay_from(conv.history, &prefix)
    } else {
        conv.history.len()
    };
    for (index, round) in conv.history.iter().enumerate() {
        let mut content = match &round.turn {
            Some(turn) if index >= verbatim_from && !turn.content.is_empty() => {
                turn.content.clone()
            }
            _ => round
                .text
                .iter()
                .map(|t| json!({ "type": "text", "text": t }))
                .collect(),
        };
        // Calls the model did not make itself (a capability's follow-up)
        // still need their tool_use; they go after the model's own blocks,
        // which then stay exactly as it wrote them.
        for c in &round.calls {
            let present = content.iter().any(|block| {
                block.get("type").and_then(Value::as_str) == Some("tool_use")
                    && block.get("id").and_then(Value::as_str) == Some(c.id.as_str())
            });
            if !present {
                content.push(json!({
                    "type": "tool_use",
                    "id": c.id,
                    "name": tool_name(&c.capability),
                    "input": c.args,
                }));
            }
        }
        messages.push(json!({ "role": "assistant", "content": content }));
        let results: Vec<Value> = round
            .results
            .iter()
            .map(|r| {
                json!({
                    "type": "tool_result",
                    "tool_use_id": r.id,
                    "content": crate::tool_output_for_model(&r.content),
                    "is_error": !r.ok,
                })
            })
            .collect();
        messages.push(json!({ "role": "user", "content": results }));
    }
    // Each round resends everything before it. One breakpoint at the end of
    // the newest message lets the next round read the whole conversation so
    // far from the cache; earlier breakpoints stay valid read points.
    if let Some(last) = messages
        .last_mut()
        .and_then(|m| m.get_mut("content"))
        .and_then(Value::as_array_mut)
        .and_then(|blocks| blocks.last_mut())
    {
        last["cache_control"] = json!({ "type": "ephemeral" });
    }
    let body = json!({
        "model": cfg.model,
        "max_tokens": cfg.max_tokens,
        // Tools render before the system prompt, so this breakpoint caches
        // both. It only pays off while the system prompt stays byte-identical
        // between rounds, which is why the butler does not put a ticking
        // clock in it.
        "system": [{
            "type": "text",
            "text": conv.system,
            "cache_control": { "type": "ephemeral" },
        }],
        "output_config": { "effort": cfg.effort },
        "fallbacks": "default",
        "tools": tools,
        "messages": messages,
    });
    (body, prefix)
}

/// Whether a rejected request was refused for the reasoning it sent back.
fn rejects_replayed_thinking(error: &str) -> bool {
    error.contains("HTTP 400") && error.contains("thinking")
}

/// The record of a turn: its content when it holds reasoning, and always the
/// prefix it was produced under. A turn without reasoning is rebuilt exactly
/// from its text and calls, so its content is not kept; its prefix still
/// tells whether the turns after it can be replayed.
fn model_turn(body: &Value, prefix: String) -> Option<crate::ModelTurn> {
    let content = body.get("content")?.as_array()?;
    let thinks = content.iter().any(|block| {
        matches!(
            block.get("type").and_then(Value::as_str),
            Some("thinking" | "redacted_thinking")
        )
    });
    Some(crate::ModelTurn {
        content: if thinks { content.clone() } else { Vec::new() },
        prefix,
    })
}

/// Turn a Messages API response into a plan.
pub fn parse_response(body: &Value) -> Plan {
    if body.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
        return Plan::failed("模型拒绝了这个请求。");
    }
    if let Some(reason) = body.get("stop_reason") {
        if !matches!(
            reason.as_str(),
            Some("end_turn" | "tool_use" | "stop_sequence")
        ) {
            return Plan::failed("模型响应未正常完成，已放弃整个计划。");
        }
    }
    let Some(content) = body.get("content").and_then(Value::as_array) else {
        return Plan::failed("模型没有返回有效内容。");
    };
    let mut steps = Vec::new();
    let mut text = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                let Some(name) = block
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                else {
                    return Plan::failed("模型返回的工具调用缺少名称，已放弃整个计划。");
                };
                let Some(args @ Value::Object(_)) = block.get("input") else {
                    return Plan::failed(format!(
                        "模型返回的工具参数无效（{name}），已放弃整个计划。"
                    ));
                };
                steps.push(Step {
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    capability: capability_name(name),
                    args: args.clone(),
                });
            }
            Some("text") => {
                let Some(t) = block.get("text").and_then(Value::as_str) else {
                    return Plan::failed("模型返回的文本格式无效，已放弃整个计划。");
                };
                if !t.trim().is_empty() {
                    text.push(t.trim().to_string());
                }
            }
            // Thinking blocks are not executable plan steps.
            Some("thinking") if block.get("thinking").is_some_and(Value::is_string) => {}
            Some("redacted_thinking") if block.get("data").is_some_and(Value::is_string) => {}
            _ => return Plan::failed("模型返回的内容块格式无效，已放弃整个计划。"),
        }
    }
    if body.get("stop_reason").and_then(Value::as_str) == Some("tool_use") && steps.is_empty() {
        return Plan::failed("模型响应缺少完整的工具调用，已放弃整个计划。");
    }
    let reply = if text.is_empty() {
        None
    } else {
        Some(text.join("\n"))
    };
    // A complete response with neither calls nor text is left to the agent:
    // after earlier rounds it simply ends the turn; as a first answer it
    // means the request went unhandled.
    Plan {
        steps,
        reply,
        ..Plan::default()
    }
}

/// What a Messages API response says it consumed. Cached input is input the
/// model read all the same, so it is counted.
pub fn response_usage(model: &str, body: &Value) -> Usage {
    let count = |name: &str| {
        body.pointer(&format!("/usage/{name}"))
            .and_then(Value::as_u64)
    };
    let input = count("input_tokens").map(|fresh| {
        fresh
            + count("cache_creation_input_tokens").unwrap_or(0)
            + count("cache_read_input_tokens").unwrap_or(0)
    });
    Usage {
        // A fallback may have answered instead of the requested model.
        model: Some(
            body.get("model")
                .and_then(Value::as_str)
                .unwrap_or(model)
                .to_string(),
        ),
        input_tokens: input,
        output_tokens: count("output_tokens"),
        cached_input_tokens: count("cache_read_input_tokens"),
    }
}

pub struct ClaudePlanner {
    cfg: ClaudeConfig,
    http: reqwest::Client,
    retry: Retry,
}

impl ClaudePlanner {
    pub fn new(cfg: ClaudeConfig) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()?;
        Ok(Self::with_client(cfg, http))
    }

    /// Use a caller-built HTTP client (custom proxy, TLS or timeout settings).
    pub fn with_client(cfg: ClaudeConfig, http: reqwest::Client) -> Self {
        Self {
            cfg,
            http,
            retry: Retry::default(),
        }
    }

    /// How rate limits and overloaded or unreachable servers are retried.
    pub fn with_retry(mut self, retry: Retry) -> Self {
        self.retry = retry;
        self
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    async fn call(&self, body: &Value) -> Result<Value, String> {
        let url = format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'));
        let resp = send_with_retry(self.retry, || {
            self.http
                .post(&url)
                .header("x-api-key", &self.cfg.api_key)
                .header("anthropic-version", API_VERSION)
                .header("anthropic-beta", FALLBACK_BETA)
                .json(body)
        })
        .await
        .map_err(|e| format!("无法连接 Claude API：{e}"))?;
        let status = resp.status();
        let value: Value = resp
            .json()
            .await
            .map_err(|e| format!("Claude API 返回了无法解析的响应（HTTP {status}）：{e}"))?;
        if !status.is_success() {
            let msg = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            return Err(format!("Claude API 错误（HTTP {status}）：{msg}"));
        }
        Ok(value)
    }
}

#[async_trait]
impl TextModel for ClaudePlanner {
    async fn complete(&self, system: &str, user: &str) -> Result<String, String> {
        self.complete_metered(system, user).await.0
    }

    async fn complete_metered(
        &self,
        system: &str,
        user: &str,
    ) -> (Result<String, String>, Option<Usage>) {
        let body = json!({
            "model": self.cfg.model,
            "max_tokens": self.cfg.max_tokens,
            "system": system,
            "output_config": { "effort": self.cfg.effort },
            "fallbacks": "default",
            "messages": [{ "role": "user", "content": user }],
        });
        let resp = match self.call(&body).await {
            Ok(resp) => resp,
            Err(error) => return (Err(error), Some(Usage::unknown(&self.cfg.model))),
        };
        let usage = Some(response_usage(&self.cfg.model, &resp));
        if resp.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
            return (Err("模型拒绝了这个请求。".into()), usage);
        }
        let text: String = resp
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect();
        (Ok(text), usage)
    }
}

#[async_trait]
impl Planner for ClaudePlanner {
    async fn plan(&self, intent: &Intent, available: &[CapabilitySpec]) -> Plan {
        self.plan_turn(&Conversation {
            system: SYSTEM_PROMPT,
            intent,
            history: &[],
            available,
        })
        .await
    }

    async fn plan_turn(&self, conv: &Conversation<'_>) -> Plan {
        let (body, prefix) = build(&self.cfg, conv, true);
        let mut answer = self.call(&body).await;
        let replayed = conv
            .history
            .iter()
            .any(|round| round.turn.as_ref().is_some_and(|t| !t.content.is_empty()));
        if replayed && answer.as_ref().is_err_and(|e| rejects_replayed_thinking(e)) {
            // The documented recovery: send the history once more without
            // any reasoning blocks. The turn loses the reasoning they
            // carried, not the task. The rejection is decided before any
            // output, so the failed attempt is not metered.
            answer = self.call(&build(&self.cfg, conv, false).0).await;
        }
        // A refusal or an unusable answer was paid for like any other, and
        // a request that failed may have been: neither is recorded as free.
        match answer {
            Ok(resp) => Plan {
                usage: Some(response_usage(&self.cfg.model, &resp)),
                turn: model_turn(&resp, prefix),
                ..parse_response(&resp)
            },
            Err(e) => Plan {
                usage: Some(Usage::unknown(&self.cfg.model)),
                ..Plan::failed(e)
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::one_shot_server;
    use agentos_core::{now_ms, CapabilityName, DeviceId, IntentId, RiskLevel, Urgency};

    fn cfg(base_url: &str) -> ClaudeConfig {
        ClaudeConfig {
            api_key: "test-key".into(),
            model: DEFAULT_MODEL.into(),
            base_url: base_url.into(),
            effort: "medium".into(),
            max_tokens: 1024,
        }
    }

    fn intent(text: &str) -> Intent {
        Intent {
            id: IntentId(1),
            source: DeviceId::new("t"),
            speaker: None,
            text: text.into(),
            urgency: Urgency::Normal,
            at_ms: now_ms(),
        }
    }

    fn specs() -> Vec<CapabilitySpec> {
        vec![CapabilitySpec {
            name: CapabilityName::new("notes.append"),
            description: "追加一条笔记".into(),
            risk: RiskLevel::Reversible,
            input_schema: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
        }]
    }

    #[test]
    fn request_offers_capabilities_as_tools() {
        let body = build_request(&cfg("x"), &intent("记一下买牛奶"), &specs());
        assert_eq!(body["model"], DEFAULT_MODEL);
        assert_eq!(body["tools"][0]["name"], "notes__append");
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["messages"][0]["content"][0]["text"], "记一下买牛奶");
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn requests_mark_the_stable_prefix_and_the_newest_turn_for_caching() {
        // Guards the cost of the planning loop: tools + system and the
        // conversation so far are read from the prompt cache on the next round.
        use crate::{Round, ToolResult};
        let i = intent("记一下买牛奶");
        let round = Round {
            text: None,
            calls: vec![Step {
                id: "t1".into(),
                capability: CapabilityName::new("notes.append"),
                args: json!({"text": "买牛奶"}),
            }],
            results: vec![ToolResult {
                id: "t1".into(),
                ok: true,
                content: "{}".into(),
            }],
            turn: None,
        };
        let request = |history: &[Round]| {
            build_turn_request(
                &cfg("x"),
                &Conversation {
                    system: "sys",
                    intent: &i,
                    history,
                    available: &specs(),
                },
            )
        };
        let first = request(&[]);
        assert_eq!(first["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(
            first["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        let second = request(std::slice::from_ref(&round));
        let m = second["messages"].as_array().unwrap();
        assert_eq!(m[2]["content"][0]["cache_control"]["type"], "ephemeral");
        // Only the newest turn carries the moving breakpoint (at most four
        // are allowed per request).
        assert!(m[0]["content"][0].get("cache_control").is_none());
        // Apart from the moving marker, the earlier request is an exact
        // prefix of the later one: nothing is rebuilt between rounds.
        assert_eq!(first["system"], second["system"]);
        assert_eq!(first["tools"], second["tools"]);
        let mut opening = first["messages"][0].clone();
        opening["content"][0]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        assert_eq!(opening, m[0]);
    }

    #[test]
    fn an_oversized_tool_result_reaches_the_model_shortened_and_marked() {
        use crate::{Round, ToolResult, MAX_TOOL_OUTPUT_CHARS};
        let i = intent("看看这个网页");
        let page = "页".repeat(MAX_TOOL_OUTPUT_CHARS * 3);
        let history = vec![Round {
            text: None,
            calls: vec![Step {
                id: "t1".into(),
                capability: CapabilityName::new("notes.append"),
                args: json!({}),
            }],
            results: vec![ToolResult {
                id: "t1".into(),
                ok: true,
                content: page,
            }],
            turn: None,
        }];
        let body = build_turn_request(
            &cfg("x"),
            &Conversation {
                system: "sys",
                intent: &i,
                history: &history,
                available: &specs(),
            },
        );
        let shown = body["messages"][2]["content"][0]["content"]
            .as_str()
            .unwrap();
        assert!(shown.chars().count() < MAX_TOOL_OUTPUT_CHARS + 100);
        // The model is told it did not see everything, never silently cut.
        assert!(shown.contains("中间省略了 40000 个字符"));
    }

    #[test]
    fn history_is_replayed_as_tool_use_and_tool_result() {
        use crate::{Round, ToolResult};
        let i = intent("记一下买牛奶");
        let history = vec![Round {
            text: Some("好的".into()),
            calls: vec![Step {
                id: "t1".into(),
                capability: CapabilityName::new("notes.append"),
                args: json!({"text": "买牛奶"}),
            }],
            results: vec![ToolResult {
                id: "t1".into(),
                ok: false,
                content: "磁盘满了".into(),
            }],
            turn: None,
        }];
        let body = build_turn_request(
            &cfg("x"),
            &Conversation {
                system: "sys",
                intent: &i,
                history: &history,
                available: &specs(),
            },
        );
        assert_eq!(body["system"][0]["text"], "sys");
        let m = &body["messages"];
        assert_eq!(m.as_array().unwrap().len(), 3);
        assert_eq!(m[1]["role"], "assistant");
        assert_eq!(m[1]["content"][0]["text"], "好的");
        assert_eq!(m[1]["content"][1]["type"], "tool_use");
        assert_eq!(m[1]["content"][1]["id"], "t1");
        assert_eq!(m[1]["content"][1]["name"], "notes__append");
        assert_eq!(m[2]["role"], "user");
        assert_eq!(m[2]["content"][0]["type"], "tool_result");
        assert_eq!(m[2]["content"][0]["tool_use_id"], "t1");
        assert_eq!(m[2]["content"][0]["is_error"], true);
    }

    #[test]
    fn tool_use_blocks_become_steps_in_order() {
        let resp = json!({
            "stop_reason": "tool_use",
            "content": [
                {"type": "text", "text": "好的，我来记下。"},
                {"type": "tool_use", "id": "t1", "name": "notes__append", "input": {"text": "买牛奶"}},
                {"type": "tool_use", "id": "t2", "name": "notes__list", "input": {}}
            ]
        });
        let plan = parse_response(&resp);
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].capability.as_str(), "notes.append");
        assert_eq!(plan.steps[0].id, "t1");
        assert_eq!(plan.steps[0].args, json!({"text": "买牛奶"}));
        assert_eq!(plan.steps[1].capability.as_str(), "notes.list");
        assert_eq!(plan.reply.as_deref(), Some("好的，我来记下。"));
    }

    #[test]
    fn refusal_yields_no_steps() {
        let resp = json!({"stop_reason": "refusal", "content": []});
        let plan = parse_response(&resp);
        assert!(plan.steps.is_empty());
        assert!(plan.reply.is_some());
    }

    fn local_planner(url: &str) -> ClaudePlanner {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        ClaudePlanner::with_client(cfg(url), http)
    }

    #[tokio::test]
    async fn plans_over_http_with_required_headers() {
        let (url, server) = one_shot_server(
            "200 OK",
            json!({"stop_reason": "tool_use", "content": [
                {"type": "tool_use", "id": "t1", "name": "notes__append", "input": {"text": "买牛奶"}}
            ]}),
        )
        .await;
        let planner = local_planner(&url);
        let plan = planner.plan(&intent("记一下买牛奶"), &specs()).await;
        assert_eq!(plan.steps[0].capability.as_str(), "notes.append");
        let req = server.await.unwrap().to_ascii_lowercase();
        assert!(req.starts_with("post /v1/messages"));
        assert!(req.contains("x-api-key: test-key"));
        assert!(req.contains(&format!("anthropic-version: {API_VERSION}")));
        assert!(req.contains(&format!("anthropic-beta: {FALLBACK_BETA}")));
    }

    #[tokio::test]
    async fn api_error_becomes_reply() {
        let (url, _server) = one_shot_server(
            "401 Unauthorized",
            json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}}),
        )
        .await;
        let planner = local_planner(&url);
        let plan = planner.plan(&intent("hi"), &specs()).await;
        assert!(plan.steps.is_empty());
        assert!(plan.reply.unwrap().contains("invalid x-api-key"));
    }

    #[tokio::test]
    async fn overloaded_api_is_retried() {
        use crate::test_http::sequence_server;
        let (url, server) = sequence_server(vec![
            (
                "529 Overloaded",
                json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
            ),
            (
                "200 OK",
                json!({"stop_reason": "tool_use", "content": [
                    {"type": "tool_use", "id": "t1", "name": "notes__append", "input": {"text": "买牛奶"}}
                ]}),
            ),
        ])
        .await;
        let planner = local_planner(&url).with_retry(Retry {
            attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        });
        let plan = planner.plan(&intent("记一下买牛奶"), &specs()).await;
        assert_eq!(plan.steps[0].capability.as_str(), "notes.append");
        assert_eq!(server.await.unwrap().len(), 2);
    }

    /// A turn as Claude returns it with reasoning between two tool calls,
    /// recorded under the prefix of `system`.
    fn thinking_round(system: &str, id: &str) -> crate::Round {
        use crate::{ModelTurn, Round, ToolResult};
        let content = vec![
            json!({"type": "thinking", "thinking": "", "signature": format!("sig-{id}-a")}),
            json!({"type": "tool_use", "id": format!("{id}-1"), "name": "notes__append", "input": {"text": "a"}}),
            json!({"type": "thinking", "thinking": "", "signature": format!("sig-{id}-b")}),
            json!({"type": "tool_use", "id": format!("{id}-2"), "name": "notes__append", "input": {"text": "b"}}),
        ];
        let tools = build_turn_request(
            &cfg("x"),
            &Conversation {
                system,
                intent: &intent("记"),
                history: &[],
                available: &specs(),
            },
        )["tools"]
            .as_array()
            .unwrap()
            .clone();
        let call = |n: &str, text: &str| Step {
            id: format!("{id}-{n}"),
            capability: CapabilityName::new("notes.append"),
            args: json!({ "text": text }),
        };
        let result = |n: &str| ToolResult {
            id: format!("{id}-{n}"),
            ok: true,
            content: "{}".into(),
        };
        Round {
            text: None,
            calls: vec![call("1", "a"), call("2", "b")],
            results: vec![result("1"), result("2")],
            turn: Some(ModelTurn {
                content,
                prefix: prefix_of(system, &tools),
            }),
        }
    }

    fn request_with(system: &str, history: &[crate::Round]) -> Value {
        build_turn_request(
            &cfg("x"),
            &Conversation {
                system,
                intent: &intent("记"),
                history,
                available: &specs(),
            },
        )
    }

    #[test]
    fn a_turn_with_reasoning_goes_back_exactly_as_the_model_wrote_it() {
        // Guards preserved thinking: blocks must return unchanged and in
        // place, interleaved reasoning included, or the service rejects them.
        let round = thinking_round("sys", "r1");
        let body = request_with("sys", std::slice::from_ref(&round));
        assert_eq!(
            body["messages"][1]["content"],
            Value::Array(round.turn.clone().unwrap().content)
        );
    }

    #[test]
    fn reasoning_from_under_an_older_prompt_is_dropped_from_the_front_only() {
        // A new stretch changed the system prompt: the earlier turn's blocks
        // can no longer be valid and go, the newer turn's stay. Dropping a
        // leading run is accepted; dropping one in the middle is not.
        let history = vec![thinking_round("old", "r1"), thinking_round("new", "r2")];
        let body = request_with("new", &history);
        let m = body["messages"].as_array().unwrap();
        let types = |message: &Value| -> Vec<String> {
            message["content"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b["type"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(types(&m[1]), vec!["tool_use", "tool_use"]);
        assert_eq!(
            types(&m[3]),
            vec!["thinking", "tool_use", "thinking", "tool_use"]
        );
        // And once the prompt changes again, nothing is replayed.
        let body = request_with("newer", &history);
        assert!(!body.to_string().contains("\"thinking\""));
    }

    #[test]
    fn a_follow_up_call_is_added_after_the_models_own_blocks() {
        let mut round = thinking_round("sys", "r1");
        round.calls.push(Step {
            id: "call-9".into(),
            capability: CapabilityName::new("notes.append"),
            args: json!({"text": "c"}),
        });
        round.results.push(crate::ToolResult {
            id: "call-9".into(),
            ok: true,
            content: "{}".into(),
        });
        let original = round.turn.clone().unwrap().content;
        let body = request_with("sys", std::slice::from_ref(&round));
        let content = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(&content[..original.len()], &original[..]);
        assert_eq!(content[original.len()]["id"], "call-9");
        // Every result has its call: three tool_use, three tool_result.
        assert_eq!(body["messages"][2]["content"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn rejected_reasoning_is_dropped_and_the_round_asked_once_more() {
        use crate::test_http::sequence_server;
        let (url, server) = sequence_server(vec![
            (
                "400 Bad Request",
                json!({"type": "error", "error": {"type": "invalid_request_error",
                    "message": "messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation."}}),
            ),
            (
                "200 OK",
                json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": "记好了"}]}),
            ),
        ])
        .await;
        let planner = local_planner(&url);
        let i = intent("记");
        let history = vec![thinking_round(SYSTEM_PROMPT, "r1")];
        let plan = planner
            .plan_turn(&Conversation {
                system: SYSTEM_PROMPT,
                intent: &i,
                history: &history,
                available: &specs(),
            })
            .await;
        assert_eq!(plan.reply.as_deref(), Some("记好了"));
        assert!(plan.error.is_none());
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("sig-r1"), "the first try replays it");
        assert!(
            !requests[1].contains("sig-r1"),
            "the retry carries no reasoning"
        );
    }

    #[test]
    fn every_turn_records_its_prefix_but_keeps_content_only_with_reasoning() {
        let plain = json!({"content": [{"type": "text", "text": "好"}]});
        let turn = model_turn(&plain, "p".into()).unwrap();
        assert!(turn.content.is_empty());
        assert_eq!(turn.prefix, "p");
        let thinking = json!({"content": [
            {"type": "thinking", "thinking": "", "signature": "s"},
            {"type": "text", "text": "好"}
        ]});
        assert_eq!(model_turn(&thinking, "p".into()).unwrap().content.len(), 2);
    }

    #[test]
    fn incomplete_or_malformed_blocks_never_run_a_partial_plan() {
        let good = json!({"type": "tool_use", "id": "t1", "name": "notes__append", "input": {"text": "x"}});
        let bad_blocks = [
            json!({"type": "tool_use", "input": {}}),
            json!({"type": "tool_use", "name": " ", "input": {}}),
            json!({"type": "tool_use", "name": "notes__clear"}),
            json!({"type": "tool_use", "name": "notes__clear", "input": []}),
            json!({"type": "tool_use", "name": "notes__clear", "input": null}),
            json!({"type": "text"}),
            json!({"type": "unknown"}),
        ];
        for bad in bad_blocks {
            let plan = parse_response(&json!({"stop_reason": "tool_use", "content": [good, bad]}));
            assert!(plan.steps.is_empty());
            assert!(plan.error.is_some());
        }
        for reason in [json!("max_tokens"), json!("pause_turn"), Value::Null] {
            let plan = parse_response(&json!({"stop_reason": reason, "content": [good]}));
            assert!(plan.steps.is_empty());
            assert!(plan.error.is_some());
        }
    }

    #[test]
    fn thinking_blocks_do_not_hide_valid_tool_calls() {
        let plan = parse_response(&json!({"stop_reason": "tool_use", "content": [
            {"type": "thinking", "thinking": "reasoning", "signature": "test"},
            {"type": "redacted_thinking", "data": "test"},
            {"type": "tool_use", "name": "notes__list", "input": {}}
        ]}));
        assert_eq!(plan.steps.len(), 1);
        assert!(plan.error.is_none());
    }
    #[test]
    fn usage_counts_cached_input_and_leaves_unreported_counts_unknown() {
        let reported = response_usage(
            "asked-for",
            &json!({"model": "answered", "usage": {
                "input_tokens": 10, "cache_creation_input_tokens": 5,
                "cache_read_input_tokens": 100, "output_tokens": 7
            }}),
        );
        assert_eq!(reported.model.as_deref(), Some("answered"));
        assert_eq!(
            (reported.input_tokens, reported.output_tokens),
            (Some(115), Some(7))
        );
        assert_eq!(reported.cached_input_tokens, Some(100));
        // Guards "unknown is not zero".
        let silent = response_usage("asked-for", &json!({"content": []}));
        assert_eq!(silent.model.as_deref(), Some("asked-for"));
        assert_eq!((silent.input_tokens, silent.output_tokens), (None, None));
        assert_eq!(silent.cached_input_tokens, None);
    }
}
