//! Requests and responses of OpenAI's Responses API (`POST /responses`).
//!
//! OpenAI's newer models only reason while using function tools on this
//! API: on Chat Completions the gpt-5.6 family refuses tools unless
//! reasoning is turned off, and a planner without reasoning repeats a tool's
//! plainly wrong answer as fact. Here the model reasons, and its reasoning
//! goes back with the next round, as the Claude planner does.
//!
//! Nothing is stored on OpenAI's side (`store: false`). The reasoning comes
//! back encrypted and is replayed with the turn that produced it, only while
//! the system prompt and tools are what they were then; earlier rounds are
//! rebuilt from their text and calls. Every capability is a function tool;
//! its calls become plan steps and go through the policy engine like any
//! other.

use serde_json::{json, Value};

use crate::openai::OpenAiConfig;
use crate::{capability_name, tool_name, Conversation, ModelTurn, Plan, Step, Usage};

/// The body for one planning round.
pub fn build_turn_request(cfg: &OpenAiConfig, conv: &Conversation<'_>) -> Value {
    build(cfg, conv, true).0
}

fn tools_of(conv: &Conversation<'_>) -> Vec<Value> {
    conv.available
        .iter()
        .map(|s| {
            json!({
                "type": "function",
                "name": tool_name(&s.name),
                "description": format!("{}（风险等级：{:?}）", s.description, s.risk),
                "parameters": s.input_schema,
                // Capability schemas are not written for strict mode (it
                // demands every property be required); policy checks the
                // arguments anyway.
                "strict": false,
            })
        })
        .collect()
}

/// The body, and the prefix it was built under. With `replay` off, every
/// round is rebuilt without its reasoning, which the service always accepts.
pub(crate) fn build(cfg: &OpenAiConfig, conv: &Conversation<'_>, replay: bool) -> (Value, String) {
    let tools = tools_of(conv);
    let prefix = crate::prefix_of(conv.system, &tools);
    let mut user = conv.intent.text.clone();
    if let Some(speaker) = &conv.intent.speaker {
        user = format!("[{speaker}] {user}");
    }
    let mut input = vec![json!({ "role": "user", "content": user })];
    let verbatim_from = if replay {
        crate::replay_from(conv.history, &prefix)
    } else {
        conv.history.len()
    };
    for (index, round) in conv.history.iter().enumerate() {
        let mut items: Vec<Value> = match &round.turn {
            Some(turn) if index >= verbatim_from && !turn.content.is_empty() => {
                turn.content.clone()
            }
            _ => round
                .text
                .iter()
                .map(|text| json!({ "role": "assistant", "content": text }))
                .collect(),
        };
        // Calls the model did not make itself (a capability's follow-up)
        // still need their function_call item; they go after the model's
        // own items, which stay exactly as it wrote them.
        for call in &round.calls {
            let present = items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call")
                    && item.get("call_id").and_then(Value::as_str) == Some(call.id.as_str())
            });
            if !present {
                items.push(json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": tool_name(&call.capability),
                    "arguments": call.args.to_string(),
                }));
            }
        }
        input.extend(items);
        for result in &round.results {
            let shown = crate::tool_output_for_model(&result.content);
            let output = if result.ok {
                shown.into_owned()
            } else {
                format!("错误：{shown}")
            };
            input.push(json!({
                "type": "function_call_output",
                "call_id": result.id,
                "output": output,
            }));
        }
    }
    let mut body = json!({
        "model": cfg.model,
        "instructions": conv.system,
        "input": input,
        // Nothing about the user's tasks is kept on the service; the
        // reasoning it would otherwise keep comes back encrypted instead.
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = json!("auto");
    }
    (body, prefix)
}

/// The record of a turn: its output items when they hold reasoning, and
/// always the prefix it was produced under. A turn without reasoning is
/// rebuilt exactly from its text and calls, so its items are not kept.
pub(crate) fn model_turn(body: &Value, prefix: String) -> Option<ModelTurn> {
    let output = body.get("output")?.as_array()?;
    let reasons = output
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"));
    Some(ModelTurn {
        content: if reasons { output.clone() } else { Vec::new() },
        prefix,
    })
}

pub fn parse_response(body: &Value) -> Plan {
    // Anything short of complete (cut off at the output limit, failed, still
    // in progress) may hold half a plan; none of it may run.
    match body.get("status").and_then(Value::as_str) {
        Some("completed") => {}
        Some("incomplete") => return Plan::failed("模型响应未正常完成，已放弃整个计划。"),
        _ => return Plan::failed("模型没有返回完整的响应，已放弃整个计划。"),
    }
    let Some(output) = body.get("output").and_then(Value::as_array) else {
        return Plan::failed("模型没有返回任何内容。");
    };
    let mut steps = Vec::new();
    let mut texts = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let Some(name) = item
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                else {
                    return Plan::failed("模型返回的工具调用缺少名称，已放弃整个计划。");
                };
                let Some(call_id) = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                else {
                    return Plan::failed("模型返回的工具调用缺少编号，已放弃整个计划。");
                };
                let args = match item.get("arguments") {
                    Some(Value::String(s)) if s.trim().is_empty() => Ok(json!({})),
                    Some(Value::String(s)) => serde_json::from_str::<Value>(s),
                    _ => {
                        return Plan::failed(format!(
                            "模型返回的工具参数无效（{name}），已放弃整个计划。"
                        ))
                    }
                };
                match args {
                    Ok(args) if args.is_object() => steps.push(Step {
                        id: call_id.to_string(),
                        capability: capability_name(name),
                        args,
                    }),
                    _ => {
                        return Plan::failed(format!(
                            "模型返回的工具参数无法解析为对象（{name}），已放弃整个计划。"
                        ))
                    }
                }
            }
            Some("message") => {
                let Some(parts) = item.get("content").and_then(Value::as_array) else {
                    return Plan::failed("模型返回的消息格式无效，已放弃整个计划。");
                };
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => match part.get("text").and_then(Value::as_str) {
                            Some(text) => texts.push(text.to_string()),
                            None => {
                                return Plan::failed("模型返回的文本格式无效，已放弃整个计划。")
                            }
                        },
                        Some("refusal") => {
                            let refusal = part
                                .get("refusal")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            return Plan::failed(format!("模型拒绝了这个请求：{refusal}"));
                        }
                        _ => return Plan::failed("模型返回的消息格式无效，已放弃整个计划。"),
                    }
                }
            }
            // Its line of thought; it goes back through the turn record.
            Some("reasoning") => {}
            // No other tool kinds are offered, so anything else is a
            // response this planner does not understand.
            _ => return Plan::failed("模型返回了不支持的内容，已放弃整个计划。"),
        }
    }
    let reply = Some(texts.join("\n").trim().to_string()).filter(|text| !text.is_empty());
    // A complete response with neither calls nor text is left to the agent,
    // as with the other planners.
    Plan {
        steps,
        reply,
        ..Plan::default()
    }
}

/// What a Responses API answer says it consumed. Compatible services may
/// leave `usage` out; the counts are then unknown.
pub fn response_usage(model: &str, body: &Value) -> Usage {
    let count = |path: &str| body.pointer(path).and_then(Value::as_u64);
    Usage {
        model: Some(
            body.get("model")
                .and_then(Value::as_str)
                .unwrap_or(model)
                .to_string(),
        ),
        input_tokens: count("/usage/input_tokens"),
        output_tokens: count("/usage/output_tokens"),
        cached_input_tokens: count("/usage/input_tokens_details/cached_tokens"),
    }
}

/// A plain completion: one system prompt and one user message, no tools.
pub fn text_request(cfg: &OpenAiConfig, system: &str, user: &str) -> Value {
    json!({
        "model": cfg.model,
        "instructions": system,
        "input": [{ "role": "user", "content": user }],
        "store": false,
    })
}

/// The text of a plain completion.
pub fn output_text(body: &Value) -> Result<String, String> {
    let plan = parse_response(body);
    if let Some(error) = plan.error {
        return Err(error);
    }
    Ok(plan.reply.unwrap_or_default())
}

/// Whether a rejected request was refused for the reasoning it sent back.
pub(crate) fn rejects_replayed_reasoning(status: reqwest::StatusCode, error: &Value) -> bool {
    let message = error
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    status == reqwest::StatusCode::BAD_REQUEST
        && (message.contains("reasoning") || message.contains("encrypted"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Round, ToolResult};
    use agentos_core::{now_ms, CapabilityName, CapabilitySpec, DeviceId, Intent, IntentId};
    use agentos_core::{RiskLevel, Urgency};

    fn cfg() -> OpenAiConfig {
        OpenAiConfig {
            api_key: None,
            base_url: "http://localhost/v1".into(),
            model: "test-model".into(),
            api: crate::openai::OpenAiApi::Responses,
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

    fn conversation<'a>(
        system: &'a str,
        intent: &'a Intent,
        history: &'a [Round],
        available: &'a [CapabilitySpec],
    ) -> Conversation<'a> {
        Conversation {
            system,
            intent,
            history,
            available,
        }
    }

    /// A turn that reasoned, then called notes.append, under `system`.
    fn reasoning_round(system: &str, available: &[CapabilitySpec]) -> Round {
        let output = vec![
            json!({"id": "rs_1", "type": "reasoning", "summary": [], "encrypted_content": "opaque"}),
            json!({"id": "fc_1", "type": "function_call", "call_id": "call_1",
                   "name": "notes__append", "arguments": "{\"text\":\"x\"}", "status": "completed"}),
        ];
        let intent = intent("x");
        let tools = tools_of(&conversation(system, &intent, &[], available));
        Round {
            text: None,
            calls: vec![Step {
                id: "call_1".into(),
                capability: CapabilityName::new("notes.append"),
                args: json!({"text": "x"}),
            }],
            results: vec![ToolResult {
                id: "call_1".into(),
                ok: true,
                content: "{\"message\":\"已记下\"}".into(),
            }],
            turn: Some(ModelTurn {
                content: output,
                prefix: crate::prefix_of(system, &tools),
            }),
        }
    }

    #[test]
    fn the_request_stores_nothing_and_offers_capabilities_as_function_tools() {
        let intent = intent("记一下 x");
        let available = specs();
        let body = build_turn_request(&cfg(), &conversation("系统", &intent, &[], &available));
        assert_eq!(body["store"], false);
        assert_eq!(body["instructions"], "系统");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["tools"][0]["name"], "notes__append");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["input"][0]["content"], "记一下 x");
        let none = build_turn_request(&cfg(), &conversation("系统", &intent, &[], &[]));
        assert!(none.get("tools").is_none());
    }

    #[test]
    fn a_reasoning_turn_goes_back_exactly_as_the_model_wrote_it() {
        let intent = intent("记一下 x");
        let available = specs();
        let history = [reasoning_round("系统", &available)];
        let body = build_turn_request(&cfg(), &conversation("系统", &intent, &history, &available));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["encrypted_content"], "opaque");
        assert_eq!(input[2]["id"], "fc_1");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input.len(), 4, "the call is not sent twice");
    }

    #[test]
    fn a_turn_from_another_prompt_is_rebuilt_without_its_reasoning() {
        // Reasoning is only valid under the instructions and tools it was
        // produced with; a changed prompt sends the bare calls instead.
        let intent = intent("记一下 x");
        let available = specs();
        let history = [reasoning_round("旧的系统提示", &available)];
        let body = build_turn_request(
            &cfg(),
            &conversation("新的系统提示", &intent, &history, &available),
        );
        let input = body["input"].as_array().unwrap();
        assert!(input.iter().all(|item| item["type"] != "reasoning"));
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["arguments"], "{\"text\":\"x\"}");
        let (rebuilt, _) = build(
            &cfg(),
            &conversation("系统", &intent, &history, &available),
            false,
        );
        assert!(rebuilt["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["type"] != "reasoning"));
    }

    #[test]
    fn a_follow_up_and_a_failed_call_are_replayed_too() {
        let intent = intent("学会 x");
        let available = specs();
        let history = [Round {
            text: Some("先写草案".into()),
            calls: vec![Step {
                id: "call-9".into(),
                capability: CapabilityName::new("evolve.install"),
                args: json!({"draft": "d1"}),
            }],
            results: vec![ToolResult {
                id: "call-9".into(),
                ok: false,
                content: "没有 PIN".into(),
            }],
            turn: None,
        }];
        let body = build_turn_request(&cfg(), &conversation("系统", &intent, &history, &available));
        let input = body["input"].as_array().unwrap();
        assert_eq!(
            input[1],
            json!({"role": "assistant", "content": "先写草案"})
        );
        assert_eq!(input[2]["name"], "evolve__install");
        assert_eq!(input[3]["output"], "错误：没有 PIN");
    }

    #[test]
    fn calls_become_steps_and_text_becomes_the_reply() {
        let plan = parse_response(&json!({"status": "completed", "output": [
            {"type": "reasoning", "summary": []},
            {"type": "function_call", "call_id": "c1", "name": "notes__append", "arguments": "{\"text\":\"牛奶\"}"},
        ]}));
        assert!(plan.error.is_none());
        assert_eq!(plan.steps[0].id, "c1");
        assert_eq!(plan.steps[0].capability.as_str(), "notes.append");
        assert_eq!(plan.steps[0].args["text"], "牛奶");
        let plan = parse_response(&json!({"status": "completed", "output": [
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "你好"}]},
        ]}));
        assert_eq!(plan.reply.as_deref(), Some("你好"));
        assert!(plan.steps.is_empty());
    }

    #[test]
    fn incomplete_refused_or_malformed_responses_never_run_a_partial_plan() {
        let call = json!({"type": "function_call", "call_id": "c1", "name": "notes__append", "arguments": "{}"});
        let bad = [
            json!({"status": "incomplete", "output": [call]}),
            json!({"status": "failed", "output": [call]}),
            json!({"output": [call]}),
            json!({"status": "completed"}),
            json!({"status": "completed", "output": [call, {"type": "function_call", "call_id": "c2", "name": "x", "arguments": "[1]"}]}),
            json!({"status": "completed", "output": [call, {"type": "function_call", "call_id": "c2", "arguments": "{}"}]}),
            json!({"status": "completed", "output": [call, {"type": "function_call", "name": "x", "arguments": "{}"}]}),
            json!({"status": "completed", "output": [call, {"type": "web_search_call"}]}),
            json!({"status": "completed", "output": [call, {"type": "message", "content": [{"type": "refusal", "refusal": "不行"}]}]}),
        ];
        for body in bad {
            let plan = parse_response(&body);
            assert!(plan.error.is_some(), "{body}");
            assert!(plan.steps.is_empty(), "{body}");
        }
    }

    #[test]
    fn usage_counts_cached_input_and_is_unknown_when_left_out() {
        let usage = response_usage(
            "m",
            &json!({"model": "gpt-x", "usage": {"input_tokens": 100, "output_tokens": 7,
                     "input_tokens_details": {"cached_tokens": 64}}}),
        );
        assert_eq!(usage.model.as_deref(), Some("gpt-x"));
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cached_input_tokens
            ),
            (Some(100), Some(7), Some(64))
        );
        let unknown = response_usage("m", &json!({}));
        assert_eq!(unknown.input_tokens, None);
        assert_eq!(unknown.model.as_deref(), Some("m"));
    }

    #[test]
    fn only_turns_that_reasoned_are_recorded_in_full() {
        let reasoned = json!({"output": [{"type": "reasoning"}, {"type": "function_call"}]});
        assert_eq!(model_turn(&reasoned, "p".into()).unwrap().content.len(), 2);
        let plain = json!({"output": [{"type": "function_call"}]});
        assert!(model_turn(&plain, "p".into()).unwrap().content.is_empty());
    }
}
