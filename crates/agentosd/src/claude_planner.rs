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

use crate::llm::TextModel;
use crate::planner::{
    capability_name, tool_name, Conversation, Plan, Planner, Step, SYSTEM_PROMPT,
};

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
    let mut messages = vec![json!({ "role": "user", "content": user })];
    for round in conv.history {
        let mut content = Vec::new();
        if let Some(t) = &round.text {
            content.push(json!({ "type": "text", "text": t }));
        }
        for c in &round.calls {
            content.push(json!({
                "type": "tool_use",
                "id": c.id,
                "name": tool_name(&c.capability),
                "input": c.args,
            }));
        }
        messages.push(json!({ "role": "assistant", "content": content }));
        let results: Vec<Value> = round
            .results
            .iter()
            .map(|r| {
                json!({
                    "type": "tool_result",
                    "tool_use_id": r.id,
                    "content": r.content,
                    "is_error": !r.ok,
                })
            })
            .collect();
        messages.push(json!({ "role": "user", "content": results }));
    }
    json!({
        "model": cfg.model,
        "max_tokens": cfg.max_tokens,
        "system": conv.system,
        "output_config": { "effort": cfg.effort },
        "fallbacks": "default",
        "tools": tools,
        "messages": messages,
    })
}

/// Turn a Messages API response into a plan.
pub fn parse_response(body: &Value) -> Plan {
    if body.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
        return Plan {
            reply: Some("模型拒绝了这个请求。".into()),
            ..Plan::default()
        };
    }
    let mut steps = Vec::new();
    let mut text = Vec::new();
    for block in body
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                if let Some(name) = block.get("name").and_then(Value::as_str) {
                    steps.push(Step {
                        id: block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        capability: capability_name(name),
                        args: block.get("input").cloned().unwrap_or_else(|| json!({})),
                    });
                }
            }
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    if !t.trim().is_empty() {
                        text.push(t.trim().to_string());
                    }
                }
            }
            _ => {}
        }
    }
    let reply = if text.is_empty() {
        None
    } else {
        Some(text.join("\n"))
    };
    Plan {
        steps,
        reply,
        tainted: false,
    }
}

pub struct ClaudePlanner {
    cfg: ClaudeConfig,
    http: reqwest::Client,
}

impl ClaudePlanner {
    pub fn new(cfg: ClaudeConfig) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()?;
        Ok(Self { cfg, http })
    }

    /// Use a caller-built HTTP client (custom proxy, TLS or timeout settings).
    pub fn with_client(cfg: ClaudeConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    async fn call(&self, body: &Value) -> Result<Value, String> {
        let url = format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(url)
            .header("x-api-key", &self.cfg.api_key)
            .header("anthropic-version", API_VERSION)
            .header("anthropic-beta", FALLBACK_BETA)
            .json(body)
            .send()
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
        let body = json!({
            "model": self.cfg.model,
            "max_tokens": self.cfg.max_tokens,
            "system": system,
            "output_config": { "effort": self.cfg.effort },
            "fallbacks": "default",
            "messages": [{ "role": "user", "content": user }],
        });
        let resp = self.call(&body).await?;
        if resp.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
            return Err("模型拒绝了这个请求。".into());
        }
        let text: String = resp
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect();
        Ok(text)
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
        let body = build_turn_request(&self.cfg, conv);
        match self.call(&body).await {
            Ok(resp) => parse_response(&resp),
            Err(e) => Plan {
                reply: Some(e),
                ..Plan::default()
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
        assert_eq!(body["messages"][0]["content"], "记一下买牛奶");
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn history_is_replayed_as_tool_use_and_tool_result() {
        use crate::planner::{Round, ToolResult};
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
        assert_eq!(body["system"], "sys");
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
}
