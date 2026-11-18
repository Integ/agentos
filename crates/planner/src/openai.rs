//! Planner for OpenAI and OpenAI-compatible services.
//!
//! OpenAI itself is spoken to through the Responses API (see
//! [`crate::responses`]), the only one on which its newer models reason
//! while using tools. Compatible servers mostly speak only Chat Completions,
//! which this module implements; [`OpenAiApi`] chooses between the two.
//!
//! Works with OpenAI itself and with the many servers that speak the same
//! protocol (vLLM, Ollama, LM Studio, DeepSeek, Qwen/DashScope, Moonshot, ...).
//! As with the Claude planner, every capability is offered as a function tool;
//! the returned `tool_calls` become plan steps and still go through the policy
//! engine before anything runs. Earlier rounds are replayed as assistant
//! `tool_calls` followed by `tool` messages.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agentos_core::{CapabilitySpec, Intent};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::llm::{send_with_retry, Retry, TextModel};
use crate::{capability_name, tool_name, Conversation, Plan, Planner, Step, Usage, SYSTEM_PROMPT};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// Sent as `Authorization: Bearer`. Local servers often need none.
    pub api_key: Option<String>,
    /// Base URL including the version segment, e.g. `https://api.openai.com/v1`
    /// or `http://localhost:11434/v1`.
    pub base_url: String,
    pub model: String,
    /// Which of the two request formats to speak.
    pub api: OpenAiApi,
}

/// The request format: OpenAI's Responses API, or Chat Completions, which
/// most compatible servers (Ollama, vLLM, DeepSeek, Qwen …) implement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiApi {
    Responses,
    ChatCompletions,
}

impl OpenAiApi {
    /// `responses` or `chat` (also `chat_completions`, `chat-completions`).
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "responses" => Some(Self::Responses),
            "chat" | "chat_completions" | "chat-completions" => Some(Self::ChatCompletions),
            _ => None,
        }
    }

    /// Responses for OpenAI's own service, Chat Completions for any other
    /// base URL: a compatible server cannot be assumed to have Responses.
    pub fn default_for(base_url: &str) -> Self {
        let host = base_url
            .split_once("://")
            .map_or(base_url, |(_, rest)| rest)
            .split(['/', ':'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if host == "api.openai.com" {
            Self::Responses
        } else {
            Self::ChatCompletions
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Responses => "Responses API",
            Self::ChatCompletions => "Chat Completions",
        }
    }
}

impl OpenAiConfig {
    /// Build from `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `OPENAI_MODEL` and
    /// `OPENAI_API`. Returns `None` unless a model is set and either a key or
    /// a base URL is. An `OPENAI_API` that names no format falls back to the
    /// default here; [`OpenAiConfig::api_from_env`] reports it.
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let api_key = get("OPENAI_API_KEY");
        let base_url = get("OPENAI_BASE_URL");
        if api_key.is_none() && base_url.is_none() {
            return None;
        }
        let base_url = base_url.unwrap_or_else(|| DEFAULT_BASE_URL.into());
        let api = Self::api_from_env()
            .ok()
            .flatten()
            .unwrap_or_else(|| OpenAiApi::default_for(&base_url));
        Some(Self {
            api_key,
            base_url,
            model: get("OPENAI_MODEL")?,
            api,
        })
    }

    /// The format `OPENAI_API` asks for: `Ok(None)` when unset, an error
    /// when it names neither.
    pub fn api_from_env() -> Result<Option<OpenAiApi>, String> {
        match std::env::var("OPENAI_API")
            .ok()
            .filter(|v| !v.trim().is_empty())
        {
            None => Ok(None),
            Some(value) => OpenAiApi::parse(&value)
                .map(Some)
                .ok_or_else(|| format!("OPENAI_API={value} 无效，可选 responses 或 chat")),
        }
    }
}

pub fn build_request(cfg: &OpenAiConfig, intent: &Intent, available: &[CapabilitySpec]) -> Value {
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

pub fn build_turn_request(cfg: &OpenAiConfig, conv: &Conversation<'_>) -> Value {
    let tools: Vec<Value> = conv
        .available
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
    let mut user = conv.intent.text.clone();
    if let Some(speaker) = &conv.intent.speaker {
        user = format!("[{speaker}] {user}");
    }
    let mut messages = vec![
        json!({ "role": "system", "content": conv.system }),
        json!({ "role": "user", "content": user }),
    ];
    for round in conv.history {
        let calls: Vec<Value> = round
            .calls
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "type": "function",
                    "function": {
                        "name": tool_name(&c.capability),
                        "arguments": c.args.to_string(),
                    }
                })
            })
            .collect();
        messages.push(json!({
            "role": "assistant",
            "content": round.text,
            "tool_calls": calls,
        }));
        for r in &round.results {
            let shown = crate::tool_output_for_model(&r.content);
            let content = if r.ok {
                shown.into_owned()
            } else {
                format!("错误：{shown}")
            };
            messages.push(json!({ "role": "tool", "tool_call_id": r.id, "content": content }));
        }
    }
    let mut body = json!({ "model": cfg.model, "messages": messages });
    // Some servers reject an empty tool list.
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = json!("auto");
    }
    body
}

pub fn parse_response(body: &Value) -> Plan {
    let Some(choice) = body.pointer("/choices/0").and_then(Value::as_object) else {
        return Plan::failed("模型没有返回任何内容。");
    };
    // Legacy compatible servers may omit finish_reason. If supplied, it must
    // indicate a complete response; a truncated plan must never run a subset.
    if let Some(reason) = choice.get("finish_reason") {
        if !matches!(reason.as_str(), Some("stop" | "tool_calls")) {
            return Plan::failed("模型响应未正常完成，已放弃整个计划。");
        }
    }
    let Some(msg) = choice.get("message").and_then(Value::as_object) else {
        return Plan::failed("模型没有返回有效消息。");
    };
    if let Some(refusal) = msg.get("refusal").filter(|value| !value.is_null()) {
        let Some(refusal) = refusal.as_str() else {
            return Plan::failed("模型返回了无效的拒绝消息。");
        };
        if !refusal.is_empty() {
            return Plan::failed(format!("模型拒绝了这个请求：{refusal}"));
        }
    }
    let calls = match msg.get("tool_calls") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(calls)) => calls.as_slice(),
        Some(_) => return Plan::failed("模型返回的工具调用格式无效，已放弃整个计划。"),
    };
    let mut steps = Vec::new();
    for call in calls {
        if call.get("type").is_some_and(|kind| kind != "function") {
            return Plan::failed("模型返回了不支持的工具调用，已放弃整个计划。");
        }
        let Some(name) = call
            .pointer("/function/name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
        else {
            return Plan::failed("模型返回的工具调用缺少名称，已放弃整个计划。");
        };
        // `arguments` is a JSON-encoded string; tolerate servers that send an object.
        let args = match call.pointer("/function/arguments") {
            Some(Value::String(s)) if s.trim().is_empty() => Ok(json!({})),
            Some(Value::String(s)) => serde_json::from_str::<Value>(s),
            Some(v @ Value::Object(_)) => Ok(v.clone()),
            _ => {
                return Plan::failed(format!(
                    "模型返回的工具参数无效（{name}），已放弃整个计划。"
                ));
            }
        };
        match args {
            Ok(args) if args.is_object() => steps.push(Step {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                capability: capability_name(name),
                args,
            }),
            _ => {
                return Plan::failed(format!(
                    "模型返回的工具参数无法解析为对象（{name}），已放弃整个计划。"
                ));
            }
        }
    }
    if choice.get("finish_reason").and_then(Value::as_str) == Some("tool_calls") && steps.is_empty()
    {
        return Plan::failed("模型响应缺少完整的工具调用，已放弃整个计划。");
    }
    if msg
        .get("content")
        .is_some_and(|content| !content.is_null() && !content.is_string())
    {
        return Plan::failed("模型返回的消息内容格式无效，已放弃整个计划。");
    }
    let reply = msg
        .get("content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    // A complete response with neither calls nor text is left to the agent:
    // after earlier rounds it simply ends the turn; as a first answer it
    // means the request went unhandled.
    Plan {
        steps,
        reply,
        ..Plan::default()
    }
}

/// What a Chat Completions response says it consumed. Compatible services
/// often leave `usage` out; the counts are then unknown.
pub fn response_usage(model: &str, body: &Value) -> Usage {
    let count = |name: &str| {
        body.pointer(&format!("/usage/{name}"))
            .and_then(Value::as_u64)
    };
    Usage {
        model: Some(
            body.get("model")
                .and_then(Value::as_str)
                .unwrap_or(model)
                .to_string(),
        ),
        input_tokens: count("prompt_tokens"),
        output_tokens: count("completion_tokens"),
        // Services that cache prompts automatically report the hit here.
        cached_input_tokens: count("prompt_tokens_details/cached_tokens"),
    }
}

pub struct OpenAiPlanner {
    cfg: OpenAiConfig,
    http: reqwest::Client,
    retry: Retry,
    /// Set once the service has refused function tools at the model's
    /// default reasoning effort (OpenAI's gpt-5.6 family on Chat
    /// Completions). Learned from the error rather than from model names:
    /// other models, and other servers, reject the parameter instead.
    reasoning_none: AtomicBool,
}

/// A refused request: what to tell the user, and whether the refusal was
/// about reasoning effort combined with function tools.
struct CallError {
    message: String,
    wants_no_reasoning: bool,
    /// The service read the request and refused the reasoning replayed in
    /// it (Responses API).
    rejects_replay: bool,
}

impl From<CallError> for String {
    fn from(error: CallError) -> Self {
        error.message
    }
}

/// Whether a 400 says function tools need `reasoning_effort` turned off.
fn rejects_tools_with_reasoning(status: reqwest::StatusCode, error: &Value) -> bool {
    if status != reqwest::StatusCode::BAD_REQUEST {
        return false;
    }
    let message = error
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    message.contains("reasoning_effort") && message.contains("tools")
}

impl OpenAiPlanner {
    pub fn new(cfg: OpenAiConfig) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()?;
        Ok(Self::with_client(cfg, http))
    }

    /// Use a caller-built HTTP client (custom proxy, TLS or timeout settings).
    pub fn with_client(cfg: OpenAiConfig, http: reqwest::Client) -> Self {
        Self {
            cfg,
            http,
            retry: Retry::default(),
            reasoning_none: AtomicBool::new(false),
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

    pub fn base_url(&self) -> &str {
        &self.cfg.base_url
    }

    async fn call(&self, body: &Value) -> Result<Value, CallError> {
        let path = match self.cfg.api {
            OpenAiApi::Responses => "responses",
            OpenAiApi::ChatCompletions => "chat/completions",
        };
        let url = format!("{}/{path}", self.cfg.base_url.trim_end_matches('/'));
        let resp = send_with_retry(self.retry, || {
            let req = self.http.post(&url).json(body);
            match &self.cfg.api_key {
                Some(key) => req.bearer_auth(key),
                None => req,
            }
        })
        .await
        .map_err(|e| CallError {
            message: format!("无法连接 {}：{e}", self.cfg.base_url),
            wants_no_reasoning: false,
            rejects_replay: false,
        })?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| CallError {
            message: format!("模型服务返回了无法解析的响应（HTTP {status}）：{e}"),
            wants_no_reasoning: false,
            rejects_replay: false,
        })?;
        if !status.is_success() {
            let msg = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            return Err(CallError {
                message: format!("模型服务错误（HTTP {status}）：{msg}"),
                wants_no_reasoning: rejects_tools_with_reasoning(status, &value),
                rejects_replay: crate::responses::rejects_replayed_reasoning(status, &value),
            });
        }
        Ok(value)
    }

    /// Send a request that offers function tools. A service that refuses
    /// them at the model's default reasoning effort gets the same request
    /// once more with reasoning turned off, and every later one starts that
    /// way. A 400 is refused before any work, so the retry neither repeats
    /// side effects (planning has none) nor pays twice.
    async fn call_with_tools(&self, body: &Value) -> Result<Value, CallError> {
        if self.reasoning_none.load(Ordering::Relaxed) {
            return self.call(&without_reasoning(body)).await;
        }
        match self.call(body).await {
            Err(error) if error.wants_no_reasoning => {
                self.reasoning_none.store(true, Ordering::Relaxed);
                self.call(&without_reasoning(body)).await
            }
            other => other,
        }
    }
}

fn without_reasoning(body: &Value) -> Value {
    let mut body = body.clone();
    body["reasoning_effort"] = json!("none");
    body
}

#[async_trait]
impl TextModel for OpenAiPlanner {
    async fn complete(&self, system: &str, user: &str) -> Result<String, String> {
        self.complete_metered(system, user).await.0
    }

    async fn complete_metered(
        &self,
        system: &str,
        user: &str,
    ) -> (Result<String, String>, Option<Usage>) {
        if self.cfg.api == OpenAiApi::Responses {
            let body = crate::responses::text_request(&self.cfg, system, user);
            return match self.call(&body).await {
                Ok(resp) => (
                    crate::responses::output_text(&resp),
                    Some(crate::responses::response_usage(&self.cfg.model, &resp)),
                ),
                Err(error) => (Err(error.into()), Some(Usage::unknown(&self.cfg.model))),
            };
        }
        let body = json!({
            "model": self.cfg.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        });
        let resp = match self.call(&body).await {
            Ok(resp) => resp,
            Err(error) => return (Err(error.into()), Some(Usage::unknown(&self.cfg.model))),
        };
        let usage = Some(response_usage(&self.cfg.model, &resp));
        let Some(msg) = resp.pointer("/choices/0/message") else {
            return (Err("模型没有返回任何内容。".into()), usage);
        };
        if let Some(r) = msg.get("refusal").and_then(Value::as_str) {
            return (Err(format!("模型拒绝了这个请求：{r}")), usage);
        }
        let text = msg
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        (Ok(text), usage)
    }
}

#[async_trait]
impl Planner for OpenAiPlanner {
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
        if self.cfg.api == OpenAiApi::Responses {
            return self.plan_turn_responses(conv).await;
        }
        let body = build_turn_request(&self.cfg, conv);
        // A refusal or an unusable answer was paid for like any other, and
        // a request that failed may have been: neither is recorded as free.
        let sent = if body.get("tools").is_some() {
            self.call_with_tools(&body).await
        } else {
            self.call(&body).await
        };
        match sent {
            Ok(resp) => Plan {
                usage: Some(response_usage(&self.cfg.model, &resp)),
                ..parse_response(&resp)
            },
            Err(e) => Plan {
                usage: Some(Usage::unknown(&self.cfg.model)),
                ..Plan::failed(e.message)
            },
        }
    }
}

impl OpenAiPlanner {
    /// One planning round on the Responses API, replaying the reasoning of
    /// earlier rounds where it is still valid.
    async fn plan_turn_responses(&self, conv: &Conversation<'_>) -> Plan {
        let (body, prefix) = crate::responses::build(&self.cfg, conv, true);
        let mut answer = self.call(&body).await;
        let replayed = conv
            .history
            .iter()
            .any(|round| round.turn.as_ref().is_some_and(|t| !t.content.is_empty()));
        if replayed && answer.as_ref().is_err_and(|e| e.rejects_replay) {
            // Send the history once more without its reasoning: the round
            // loses the model's earlier line of thought, not the task. The
            // refusal comes before any output, so it is not paid for twice.
            answer = self
                .call(&crate::responses::build(&self.cfg, conv, false).0)
                .await;
        }
        match answer {
            Ok(resp) => Plan {
                usage: Some(crate::responses::response_usage(&self.cfg.model, &resp)),
                turn: crate::responses::model_turn(&resp, prefix),
                ..crate::responses::parse_response(&resp)
            },
            Err(e) => Plan {
                usage: Some(Usage::unknown(&self.cfg.model)),
                ..Plan::failed(e.message)
            },
        }
    }

    pub fn api(&self) -> OpenAiApi {
        self.cfg.api
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
            api: OpenAiApi::ChatCompletions,
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
    fn history_is_replayed_as_tool_calls_and_tool_messages() {
        use crate::{Round, ToolResult};
        let i = intent("记一下 x");
        let history = vec![Round {
            text: None,
            calls: vec![Step {
                id: "c1".into(),
                capability: CapabilityName::new("notes.append"),
                args: json!({"text": "x"}),
            }],
            results: vec![ToolResult {
                id: "c1".into(),
                ok: true,
                content: "{\"message\":\"ok\"}".into(),
            }],
            turn: None,
        }];
        let body = build_turn_request(
            &cfg("x", None),
            &Conversation {
                system: "sys",
                intent: &i,
                history: &history,
                available: &specs(),
            },
        );
        let m = &body["messages"];
        assert_eq!(m[0]["content"], "sys");
        assert_eq!(m[2]["role"], "assistant");
        assert_eq!(m[2]["tool_calls"][0]["id"], "c1");
        assert_eq!(
            m[2]["tool_calls"][0]["function"]["arguments"],
            "{\"text\":\"x\"}"
        );
        assert_eq!(m[3]["role"], "tool");
        assert_eq!(m[3]["tool_call_id"], "c1");
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

    fn responses_planner(url: &str) -> OpenAiPlanner {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut cfg = cfg(url, Some("sk-test"));
        cfg.api = OpenAiApi::Responses;
        OpenAiPlanner::with_client(cfg, http)
    }

    #[test]
    fn openai_itself_gets_the_responses_api_and_other_servers_chat_completions() {
        // Compatible servers rarely implement Responses; OpenAI's newer
        // models only reason with tools there.
        assert_eq!(
            OpenAiApi::default_for(DEFAULT_BASE_URL),
            OpenAiApi::Responses
        );
        assert_eq!(
            OpenAiApi::default_for("https://API.openai.com:443/v1/"),
            OpenAiApi::Responses
        );
        for other in [
            "http://localhost:11434/v1",
            "https://api.deepseek.com/v1",
            "https://api.openai.com.evil.example/v1",
        ] {
            assert_eq!(
                OpenAiApi::default_for(other),
                OpenAiApi::ChatCompletions,
                "{other}"
            );
        }
        assert_eq!(OpenAiApi::parse(" Responses "), Some(OpenAiApi::Responses));
        assert_eq!(OpenAiApi::parse("chat"), Some(OpenAiApi::ChatCompletions));
        assert_eq!(OpenAiApi::parse("completions"), None);
    }

    #[tokio::test]
    async fn plans_over_the_responses_api_and_keeps_the_reasoning_turn() {
        let (url, server) = one_shot_server(
            "200 OK",
            json!({"status": "completed", "model": "test-model", "output": [
                {"id": "rs_1", "type": "reasoning", "summary": [], "encrypted_content": "e"},
                {"id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "notes__append", "arguments": "{\"text\":\"x\"}"}
            ], "usage": {"input_tokens": 10, "output_tokens": 3}}),
        )
        .await;
        let plan = responses_planner(&format!("{url}/v1"))
            .plan(&intent("记一下 x"), &specs())
            .await;
        assert!(plan.error.is_none(), "{:?}", plan.error);
        assert_eq!(plan.steps[0].id, "call_1");
        assert_eq!(plan.turn.unwrap().content.len(), 2);
        assert_eq!(plan.usage.unwrap().input_tokens, Some(10));
        let req = server.await.unwrap();
        assert!(req.to_ascii_lowercase().starts_with("post /v1/responses"));
        assert!(req.contains(r#""store":false"#));
    }

    #[tokio::test]
    async fn refused_replayed_reasoning_is_dropped_and_the_round_asked_again() {
        let call = json!({"status": "completed", "output": [
            {"type": "message", "content": [{"type": "output_text", "text": "好了"}]}
        ]});
        let (url, server) = crate::test_http::sequence_server(vec![
            (
                "400 Bad Request",
                json!({"error": {"message": "The encrypted content for item rs_1 could not be verified."}}),
            ),
            ("200 OK", call),
        ])
        .await;
        let planner = responses_planner(&url);
        let intent = intent("记一下 x");
        let available = specs();
        let (_, prefix) = crate::responses::build(
            &planner.cfg,
            &Conversation {
                system: SYSTEM_PROMPT,
                intent: &intent,
                history: &[],
                available: &available,
            },
            true,
        );
        let history = [crate::Round {
            text: None,
            calls: vec![Step {
                id: "call_1".into(),
                capability: CapabilityName::new("notes.append"),
                args: json!({"text": "x"}),
            }],
            results: vec![crate::ToolResult {
                id: "call_1".into(),
                ok: true,
                content: "{}".into(),
            }],
            turn: Some(crate::ModelTurn {
                content: vec![
                    json!({"id": "rs_1", "type": "reasoning", "encrypted_content": "e"}),
                    json!({"type": "function_call", "call_id": "call_1", "name": "notes__append", "arguments": "{\"text\":\"x\"}"}),
                ],
                prefix,
            }),
        }];
        let plan = planner
            .plan_turn(&Conversation {
                system: SYSTEM_PROMPT,
                intent: &intent,
                history: &history,
                available: &available,
            })
            .await;
        assert_eq!(plan.reply.as_deref(), Some("好了"));
        let seen = server.await.unwrap();
        assert!(seen[0].contains("encrypted_content"));
        assert!(!seen[1].contains(r#""type":"reasoning""#));
        assert!(
            seen[1].contains("call_1"),
            "the calls themselves still go back"
        );
    }

    #[tokio::test]
    async fn a_model_that_refuses_tools_while_reasoning_is_asked_again_without_it() {
        // gpt-5.6 on Chat Completions refuses function tools unless
        // reasoning_effort is "none"; other models refuse that parameter,
        // so it is sent only after the service asked for it.
        let refusal = json!({"error": {
            "message": "Function tools with reasoning_effort are not supported for gpt-5.6-terra in /v1/chat/completions. To use function tools, use /v1/responses or set reasoning_effort to 'none'.",
            "type": "invalid_request_error", "param": "reasoning_effort"
        }});
        let call = json!({"choices": [{"finish_reason": "tool_calls", "message": {"tool_calls": [
            {"id": "c1", "type": "function", "function": {"name": "notes__list", "arguments": "{}"}}
        ]}}]});
        let (url, server) = crate::test_http::sequence_server(vec![
            ("400 Bad Request", refusal),
            ("200 OK", call.clone()),
            ("200 OK", call),
        ])
        .await;
        let planner = local_planner(&url, None);
        let first = planner.plan(&intent("看看笔记"), &specs()).await;
        let second = planner.plan(&intent("再看看"), &specs()).await;
        assert_eq!(first.steps[0].capability.as_str(), "notes.list");
        assert_eq!(second.steps[0].capability.as_str(), "notes.list");
        let seen = server.await.unwrap();
        assert!(!seen[0].contains("reasoning_effort"));
        assert!(seen[1].contains(r#""reasoning_effort":"none""#));
        // Learned once: the next request does not pay for another refusal.
        assert!(seen[2].contains(r#""reasoning_effort":"none""#));
    }

    #[tokio::test]
    async fn other_bad_requests_are_not_retried() {
        let (url, server) = crate::test_http::sequence_server(vec![(
            "400 Bad Request",
            json!({"error": {"message": "Unrecognized request argument supplied: reasoning_effort"}}),
        )])
        .await;
        let plan = local_planner(&url, None)
            .plan(&intent("hi"), &specs())
            .await;
        assert!(plan.steps.is_empty());
        assert!(plan.reply.unwrap().contains("Unrecognized"));
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[test]
    fn incomplete_or_malformed_tool_responses_never_run_a_partial_plan() {
        let good = json!({"type": "function", "function": {
            "name": "notes__append", "arguments": "{\"text\":\"x\"}"
        }});
        let bad_calls = [
            json!({"type": "function", "function": {"arguments": "{}"}}),
            json!({"type": "function", "function": {"name": " ", "arguments": "{}"}}),
            json!({"type": "function", "function": {"name": "notes__clear"}}),
            json!({"type": "function", "function": {"name": "notes__clear", "arguments": []}}),
            json!({"type": "function", "function": {"name": "notes__clear", "arguments": "[]"}}),
            json!({"type": "function", "function": {"name": "notes__clear", "arguments": "null"}}),
            json!({"type": "other", "function": {"name": "notes__clear", "arguments": "{}"}}),
        ];
        for bad in bad_calls {
            let plan = parse_response(&json!({"choices": [{"message": {
                "tool_calls": [good, bad]
            }}]}));
            assert!(plan.steps.is_empty());
            assert!(plan.error.is_some());
        }
        for reason in [json!("length"), json!("content_filter"), Value::Null] {
            let plan = parse_response(&json!({"choices": [{
                "finish_reason": reason,
                "message": {"tool_calls": [good]}
            }]}));
            assert!(plan.steps.is_empty());
            assert!(plan.error.is_some());
        }
    }

    #[test]
    fn complete_response_may_run_but_a_refusal_cannot() {
        let plan = parse_response(&json!({"choices": [{
            "finish_reason": "tool_calls",
            "message": {"tool_calls": [{"type": "function", "function": {
                "name": "notes__list", "arguments": "{}"
            }}]}
        }]}));
        assert_eq!(plan.steps.len(), 1);
        assert!(plan.error.is_none());
        let plan = parse_response(&json!({"choices": [{"message": {
            "refusal": "cannot do this",
            "tool_calls": [{"function": {"name": "notes__clear", "arguments": "{}"}}]
        }}]}));
        assert!(plan.steps.is_empty());
        assert!(plan.error.is_some());
    }
    #[test]
    fn usage_is_read_when_reported_and_unknown_when_a_service_leaves_it_out() {
        let reported = response_usage(
            "asked-for",
            &json!({"model": "answered", "usage": {
                "prompt_tokens": 12, "completion_tokens": 3,
                "prompt_tokens_details": {"cached_tokens": 8}
            }}),
        );
        assert_eq!(reported.model.as_deref(), Some("answered"));
        assert_eq!(
            (reported.input_tokens, reported.output_tokens),
            (Some(12), Some(3))
        );
        assert_eq!(reported.cached_input_tokens, Some(8));
        // Guards "unknown is not zero": many compatible services report nothing.
        let silent = response_usage("asked-for", &json!({"choices": []}));
        assert_eq!(silent.model.as_deref(), Some("asked-for"));
        assert_eq!((silent.input_tokens, silent.output_tokens), (None, None));
    }
}
