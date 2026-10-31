//! Planner for any OpenAI-compatible Chat Completions endpoint.
//!
//! Works with OpenAI itself and with the many servers that speak the same
//! protocol (vLLM, Ollama, LM Studio, DeepSeek, Qwen/DashScope, Moonshot, ...).
//! As with the Claude planner, every capability is offered as a function tool;
//! the returned `tool_calls` become plan steps and still go through the policy
//! engine before anything runs.

use std::time::Duration;

use agentos_core::{CapabilitySpec, Intent};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::planner::{capability_name, tool_name, Plan, Planner, Step, SYSTEM_PROMPT};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// Sent as `Authorization: Bearer`. Local servers often need none.
    pub api_key: Option<String>,
    /// Base URL including the version segment, e.g. `https://api.openai.com/v1`
    /// or `http://localhost:11434/v1`.
    pub base_url: String,
    pub model: String,
}

impl OpenAiConfig {
    /// Build from `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `OPENAI_MODEL`.
    /// Returns `None` unless a model is set and either a key or a base URL is.
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let api_key = get("OPENAI_API_KEY");
        let base_url = get("OPENAI_BASE_URL");
        if api_key.is_none() && base_url.is_none() {
            return None;
        }
        Some(Self {
            api_key,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.into()),
            model: get("OPENAI_MODEL")?,
        })
    }
}

pub fn build_request(cfg: &OpenAiConfig, intent: &Intent, available: &[CapabilitySpec]) -> Value {
    let tools: Vec<Value> = available
        .iter()
        .map(|s| {
            json!({
                "type": "function",
                "function": {
                    "name": tool_name(&s.name),
                    "description": format!("{}（风险等级：{:?}）", s.description, s.risk),
                    "parameters": s.input_schema,
                }
            })
        })
        .collect();
    let mut user = intent.text.clone();
    if let Some(speaker) = &intent.speaker {
        user = format!("[{speaker}] {user}");
    }
    let mut body = json!({
        "model": cfg.model,
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": user },
        ],
    });
    // Some servers reject an empty tool list.
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = json!("auto");
    }
    body
}

pub fn parse_response(body: &Value) -> Plan {
    let Some(msg) = body.pointer("/choices/0/message") else {
        return Plan {
            reply: Some("模型没有返回任何内容。".into()),
            ..Plan::default()
        };
    };
    let mut steps = Vec::new();
    let mut bad = Vec::new();
    for call in msg
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = call.pointer("/function/name").and_then(Value::as_str) else {
            continue;
        };
        // `arguments` is a JSON-encoded string; tolerate servers that send an object.
        let args = match call.pointer("/function/arguments") {
            Some(Value::String(s)) if s.trim().is_empty() => Ok(json!({})),
            Some(Value::String(s)) => serde_json::from_str::<Value>(s),
            Some(v @ Value::Object(_)) => Ok(v.clone()),
            _ => Ok(json!({})),
        };
        match args {
            Ok(args) => steps.push(Step {
                capability: capability_name(name),
                args,
            }),
            Err(_) => bad.push(name.to_string()),
        }
    }
    if !bad.is_empty() {
        // A half-parsed plan could run the wrong subset of steps; run none.
        return Plan {
            reply: Some(format!(
                "模型返回的工具参数无法解析（{}），已放弃整个计划。",
                bad.join("、")
            )),
            ..Plan::default()
        };
    }
    let reply = msg
        .get("content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| {
            msg.get("refusal")
                .and_then(Value::as_str)
                .map(|r| format!("模型拒绝了这个请求：{r}"))
        });
    Plan {
        steps,
        reply,
        tainted: false,
    }
}

pub struct OpenAiPlanner {
    cfg: OpenAiConfig,
    http: reqwest::Client,
}

impl OpenAiPlanner {
    pub fn new(cfg: OpenAiConfig) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()?;
        Ok(Self { cfg, http })
    }

    /// Use a caller-built HTTP client (custom proxy, TLS or timeout settings).
    pub fn with_client(cfg: OpenAiConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    pub fn base_url(&self) -> &str {
        &self.cfg.base_url
    }

    async fn call(&self, body: &Value) -> Result<Value, String> {
        let url = format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        );
        let mut req = self.http.post(url).json(body);
        if let Some(key) = &self.cfg.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("无法连接 {}：{e}", self.cfg.base_url))?;
        let status = resp.status();
        let value: Value = resp
            .json()
            .await
            .map_err(|e| format!("模型服务返回了无法解析的响应（HTTP {status}）：{e}"))?;
        if !status.is_success() {
            let msg = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            return Err(format!("模型服务错误（HTTP {status}）：{msg}"));
        }
        Ok(value)
    }
}

#[async_trait]
impl Planner for OpenAiPlanner {
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
    use crate::test_http::one_shot_server;
    use agentos_core::{now_ms, CapabilityName, DeviceId, IntentId, RiskLevel, Urgency};

    fn cfg(base_url: &str, key: Option<&str>) -> OpenAiConfig {
        OpenAiConfig {
            api_key: key.map(str::to_string),
            base_url: base_url.into(),
            model: "test-model".into(),
        }
    }

    fn local_planner(url: &str, key: Option<&str>) -> OpenAiPlanner {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        OpenAiPlanner::with_client(cfg(url, key), http)
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
    fn request_uses_function_tools_and_system_message() {
        let body = build_request(&cfg("x", None), &intent("记一下买牛奶"), &specs());
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "记一下买牛奶");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "notes__append");
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn no_capabilities_means_no_tools_field() {
        let body = build_request(&cfg("x", None), &intent("hi"), &[]);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn tool_calls_become_steps_with_parsed_arguments() {
        let resp = json!({"choices": [{"message": {
            "role": "assistant",
            "content": null,
            "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "notes__append", "arguments": "{\"text\":\"买牛奶\"}"}},
                {"id": "c2", "type": "function", "function": {"name": "notes__list", "arguments": ""}}
            ]
        }}]});
        let plan = parse_response(&resp);
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].capability.as_str(), "notes.append");
        assert_eq!(plan.steps[0].args, json!({"text": "买牛奶"}));
        assert_eq!(plan.steps[1].args, json!({}));
        assert!(plan.reply.is_none());
    }

    #[test]
    fn malformed_arguments_drop_the_whole_plan() {
        let resp = json!({"choices": [{"message": {"tool_calls": [
            {"id": "c1", "type": "function", "function": {"name": "notes__list", "arguments": "{}"}},
            {"id": "c2", "type": "function", "function": {"name": "notes__clear", "arguments": "{oops"}}
        ]}}]});
        let plan = parse_response(&resp);
        assert!(plan.steps.is_empty());
        assert!(plan.reply.unwrap().contains("notes__clear"));
    }

    #[test]
    fn plain_text_answer_is_reply() {
        let resp = json!({"choices": [{"message": {"content": " 你好！ "}}]});
        let plan = parse_response(&resp);
        assert!(plan.steps.is_empty());
        assert_eq!(plan.reply.as_deref(), Some("你好！"));
    }

    #[tokio::test]
    async fn plans_over_http_with_bearer_key() {
        let (url, server) = one_shot_server(
            "200 OK",
            json!({"choices": [{"message": {"tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "notes__append", "arguments": "{\"text\":\"x\"}"}}
            ]}}]}),
        )
        .await;
        let plan = local_planner(&format!("{url}/v1"), Some("sk-test"))
            .plan(&intent("记一下 x"), &specs())
            .await;
        assert_eq!(plan.steps[0].capability.as_str(), "notes.append");
        let req = server.await.unwrap().to_ascii_lowercase();
        assert!(req.starts_with("post /v1/chat/completions"));
        assert!(req.contains("authorization: bearer sk-test"));
    }

    #[tokio::test]
    async fn no_key_sends_no_authorization_header() {
        let (url, server) = one_shot_server(
            "200 OK",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let plan = local_planner(&url, None)
            .plan(&intent("hi"), &specs())
            .await;
        assert_eq!(plan.reply.as_deref(), Some("ok"));
        let req = server.await.unwrap().to_ascii_lowercase();
        assert!(!req.contains("authorization:"));
    }

    #[tokio::test]
    async fn api_error_becomes_reply() {
        let (url, _server) = one_shot_server(
            "404 Not Found",
            json!({"error": {"message": "model not found", "type": "invalid_request_error"}}),
        )
        .await;
        let plan = local_planner(&url, None)
            .plan(&intent("hi"), &specs())
            .await;
        assert!(plan.steps.is_empty());
        assert!(plan.reply.unwrap().contains("model not found"));
    }
}
