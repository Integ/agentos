//! Planner backed by the Claude Messages API.
//!
//! Each intent becomes one `POST /v1/messages` call. Every registered
//! capability is offered as a tool; the `tool_use` blocks in the response
//! become plan steps (the planner never executes anything itself — the butler
//! runs each step through the policy engine). Text blocks become the reply.
//!
//! Rust has no official Anthropic SDK, so this speaks raw HTTP.

use std::time::Duration;

use agentos_core::{CapabilityName, CapabilitySpec, Intent};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::planner::{Plan, Planner, Step};

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

const SYSTEM_PROMPT: &str = "你是 AgentOS 的管家 Agent，运行在用户的桌面工作站上。\
用户通过键盘或语音向你表达意图，你通过调用工具完成任务。\
\n\n- 能用工具完成的，就调用对应的工具；需要多个步骤时，按执行顺序依次调用。\
\n- 工具调用会先经过系统的权限检查，高风险操作会由系统向用户请求确认，你不需要自己再问。\
\n- 没有合适的工具时，用一两句话直接回答或说明做不到，不要编造工具。\
\n- 用用户使用的语言回复，简洁。";

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

/// Tool names must match `^[a-zA-Z0-9_-]{1,64}$`; capability names use dots.
pub fn tool_name(cap: &CapabilityName) -> String {
    cap.as_str().replace('.', "__")
}

fn capability_name(tool: &str) -> CapabilityName {
    CapabilityName::new(tool.replace("__", "."))
}

/// Request body for one intent.
pub fn build_request(cfg: &ClaudeConfig, intent: &Intent, available: &[CapabilitySpec]) -> Value {
    let tools: Vec<Value> = available
        .iter()
        .map(|s| {
            json!({
                "name": tool_name(&s.name),
                "description": format!("{}（风险等级：{:?}）", s.description, s.risk),
                "input_schema": s.input_schema,
            })
        })
        .collect();
    let mut user = intent.text.clone();
    if let Some(speaker) = &intent.speaker {
        user = format!("[{speaker}] {user}");
    }
    json!({
        "model": cfg.model,
        "max_tokens": cfg.max_tokens,
        "system": SYSTEM_PROMPT,
        "output_config": { "effort": cfg.effort },
        "fallbacks": "default",
        "tools": tools,
        "messages": [{ "role": "user", "content": user }],
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
impl Planner for ClaudePlanner {
    async fn plan(&self, intent: &Intent, available: &[CapabilitySpec]) -> Plan {
        let body = build_request(&self.cfg, intent, available);
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
    use agentos_core::{now_ms, DeviceId, IntentId, RiskLevel, Urgency};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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

    /// Serve one canned HTTP response and capture the request.
    async fn one_shot_server(
        status: &str,
        body: Value,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let payload = body.to_string();
        let status = status.to_string();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            let mut req = Vec::new();
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                req.extend_from_slice(&buf[..n]);
                let s = String::from_utf8_lossy(&req);
                if let Some(idx) = s.find("\r\n\r\n") {
                    let len = s[..idx]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if req.len() >= idx + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                payload.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&req).to_string()
        });
        (format!("http://{addr}"), handle)
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
