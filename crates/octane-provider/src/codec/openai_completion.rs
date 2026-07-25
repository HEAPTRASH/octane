//! OpenAI Chat Completions.
//!
//! The format with the longest tail of divergence, because every provider has
//! its own reading of it (`RESEARCH.md` §L). Quirks handled here rather than in
//! the loop:
//!
//! - reasoning comes back as `reasoning_content` on some providers, `reasoning`
//!   on others, and not at all on OpenAI
//! - tool call arguments stream as fragments addressed by array index, and the
//!   `id` and `name` appear only in the first fragment
//! - some providers omit `index` when there is exactly one tool call
//! - usage arrives in a final chunk with an empty `choices` array, which a naive
//!   decoder reads as a malformed message

use std::collections::HashMap;

use octane_protocol::{Part, Role, ToolCallId, Usage};
use serde_json::json;

use crate::config::ResolvedModel;
use crate::model::ModelRequest;
use crate::stream::{FinishReason, StreamEvent};
use crate::ProviderError;

#[derive(Debug, Default)]
pub struct State {
    /// Tool calls under construction, by array index.
    tools: HashMap<u64, PartialTool>,
    text_open: bool,
    reasoning_open: bool,
    usage: Usage,
    started: bool,
}

#[derive(Debug, Default)]
struct PartialTool {
    id: ToolCallId,
    name: String,
    input: String,
}

pub fn build(model: &ResolvedModel, request: &ModelRequest) -> serde_json::Value {
    let mut messages: Vec<serde_json::Value> = Vec::new();

    for message in &request.messages {
        // Tool results are their own role here, not content on a user message.
        let results: Vec<&octane_protocol::ToolResult> = message
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect();

        for result in &results {
            messages.push(json!({
                "role": "tool",
                "tool_call_id": result.call_id.as_str(),
                "content": result.output,
            }));
        }

        let text = message
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::Text { text } | Part::Synthetic { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        let tool_calls: Vec<serde_json::Value> = message
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::ToolCall(call) => Some(json!({
                    "id": call.id.as_str(),
                    "type": "function",
                    "function": { "name": call.name, "arguments": call.input },
                })),
                _ => None,
            })
            .collect();

        if text.trim().is_empty() && tool_calls.is_empty() {
            continue;
        }

        let mut entry = json!({
            "role": match message.role {
                Role::System => "system",
                Role::Assistant => "assistant",
                // `developer` is rejected by Cerebras, xAI, Mistral, and Chutes,
                // so it is never sent — `system` is understood everywhere.
                Role::Developer => "system",
                Role::User => "user",
            },
        });
        if !text.trim().is_empty() {
            entry["content"] = json!(text);
        }
        if !tool_calls.is_empty() {
            entry["tool_calls"] = json!(tool_calls);
        }
        messages.push(entry);
    }

    let mut body = json!({
        "model": model.model_id,
        "messages": messages,
        "stream": true,
        // Without this, most providers never report usage on a streamed
        // response and cost tracking silently reads zero.
        "stream_options": { "include_usage": true },
        "max_tokens": request.max_output_tokens.min(model.max_output),
    });

    if !request.tools.is_empty() {
        body["tools"] = json!(super::tool_schemas(model, request));
    }
    if let Some(temperature) = request.temperature.or(model.temperature) {
        body["temperature"] = json!(temperature);
    }
    if let Some(top_p) = request.top_p {
        body["top_p"] = json!(top_p);
    }

    // The flat `reasoning_effort` is OpenAI's spelling and is accepted by
    // OpenRouter too, verified against it — so one form covers both rather than
    // needing per-provider configuration.
    //
    // Grok rejects this field outright (`RESEARCH.md` §L). A model that does
    // should set `thinking: "auto"`, which sends nothing.
    let thinking = if request.thinking.is_auto() { model.thinking } else { request.thinking };
    if let Some(effort) = thinking.effort() {
        body["reasoning_effort"] = json!(effort);
    }
    body
}

pub fn decode(state: &mut State, event: &crate::sse::Event) -> Result<Vec<StreamEvent>, ProviderError> {
    let value: serde_json::Value = serde_json::from_str(&event.data)
        .map_err(|error| ProviderError::Transport(format!("malformed chunk: {error}")))?;

    if let Some(message) = value["error"]["message"].as_str() {
        return Err(ProviderError::Api { status: 200, message: message.to_string() });
    }

    let mut events = Vec::new();
    if !state.started {
        state.started = true;
        events.push(StreamEvent::StepStart);
    }

    // Usage rides a final chunk whose `choices` is empty. Reading it as a
    // malformed message loses cost tracking on every request.
    if let Some(usage) = value.get("usage").filter(|u| u.is_object()) {
        state.usage = read_usage(usage);
    }

    let Some(choice) = value["choices"].as_array().and_then(|c| c.first()) else {
        return Ok(events);
    };
    let delta = &choice["delta"];

    // Providers disagree on the field name, and OpenAI omits it entirely.
    let reasoning = delta["reasoning_content"]
        .as_str()
        .or_else(|| delta["reasoning"].as_str())
        .filter(|text| !text.is_empty());
    if let Some(text) = reasoning {
        if !state.reasoning_open {
            state.reasoning_open = true;
            events.push(StreamEvent::ReasoningStart);
        }
        events.push(StreamEvent::ReasoningDelta(text.to_string()));
    }

    if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
        if state.reasoning_open {
            state.reasoning_open = false;
            events.push(StreamEvent::ReasoningEnd);
        }
        if !state.text_open {
            state.text_open = true;
            events.push(StreamEvent::TextStart);
        }
        events.push(StreamEvent::TextDelta(text.to_string()));
    }

    if let Some(calls) = delta["tool_calls"].as_array() {
        for call in calls {
            // Some providers omit `index` when there is only one call.
            let index = call["index"].as_u64().unwrap_or(0);
            let entry = state.tools.entry(index).or_default();

            // id and name arrive only in the first fragment.
            if let Some(id) = call["id"].as_str().filter(|id| !id.is_empty()) {
                entry.id = ToolCallId::from(id.to_string());
            }
            if let Some(name) = call["function"]["name"].as_str().filter(|n| !n.is_empty()) {
                entry.name = name.to_string();
                events.push(StreamEvent::ToolCallStart {
                    id: entry.id.clone(),
                    name: entry.name.clone(),
                });
            }
            if let Some(fragment) = call["function"]["arguments"].as_str() {
                if !fragment.is_empty() {
                    entry.input.push_str(fragment);
                    events.push(StreamEvent::ToolCallInputDelta {
                        id: entry.id.clone(),
                        delta: fragment.to_string(),
                    });
                }
            }
        }
    }

    if let Some(reason) = choice["finish_reason"].as_str() {
        if state.text_open {
            state.text_open = false;
            events.push(StreamEvent::TextEnd);
        }
        if state.reasoning_open {
            state.reasoning_open = false;
            events.push(StreamEvent::ReasoningEnd);
        }

        // Emitted in index order so parallel calls arrive as the model meant.
        let mut pending: Vec<(u64, PartialTool)> = state.tools.drain().collect();
        pending.sort_by_key(|(index, _)| *index);
        for (_, tool) in pending {
            let input = if tool.input.trim().is_empty() { "{}".into() } else { tool.input };
            events.push(StreamEvent::ToolCallEnd { id: tool.id, input });
        }

        events.push(StreamEvent::StepFinish {
            reason: finish_reason(reason),
            usage: state.usage,
        });
    }

    Ok(events)
}

fn read_usage(usage: &serde_json::Value) -> Usage {
    let cached = usage["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or_default();
    Usage {
        input_tokens: usage["prompt_tokens"].as_u64().unwrap_or_default(),
        output_tokens: usage["completion_tokens"].as_u64().unwrap_or_default(),
        cached_input_tokens: cached,
        reasoning_tokens: usage["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .unwrap_or_default(),
        cost: 0.0,
    }
}

fn finish_reason(reason: &str) -> FinishReason {
    match reason {
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiType;
    use crate::codec::tests::{model, request};

    fn decode_all(chunks: &[&str]) -> Vec<StreamEvent> {
        let mut state = State::default();
        chunks
            .iter()
            .flat_map(|c| {
                decode(&mut state, &crate::sse::Event { name: None, data: c.to_string() }).unwrap()
            })
            .collect()
    }

    #[test]
    fn usage_is_requested_or_it_never_arrives() {
        let body = build(&model(ApiType::OpenAiCompletion), &request());
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn the_developer_role_is_never_sent() {
        // Cerebras, xAI, Mistral, and Chutes reject it.
        let mut request = request();
        request.messages.push(octane_protocol::Message::text(Role::Developer, "ctx"));
        let body = build(&model(ApiType::OpenAiCompletion), &request);

        let roles: Vec<&str> =
            body["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert!(!roles.contains(&"developer"));
        assert!(roles.contains(&"system"));
    }

    #[test]
    fn tool_results_become_their_own_role() {
        let mut request = request();
        request.messages.push(octane_protocol::Message::new(
            Role::User,
            vec![Part::ToolResult(octane_protocol::ToolResult {
                call_id: ToolCallId::from("tc_1".to_string()),
                output: "done".into(),
                metadata: None,
                is_error: false,
            })],
        ));
        let body = build(&model(ApiType::OpenAiCompletion), &request);
        let last = body["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["role"], "tool");
        assert_eq!(last["tool_call_id"], "tc_1");
    }

    #[test]
    fn text_streams_with_start_and_end() {
        let events = decode_all(&[
            r#"{"choices":[{"delta":{"content":"hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        assert_eq!(events[0], StreamEvent::StepStart);
        assert_eq!(events[1], StreamEvent::TextStart);
        assert_eq!(events[2], StreamEvent::TextDelta("hel".into()));
        assert!(events.contains(&StreamEvent::TextEnd));
    }

    #[test]
    fn tool_calls_accumulate_across_fragments() {
        let events = decode_all(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"tc_1","function":{"name":"read","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        let end = events.iter().find(|e| matches!(e, StreamEvent::ToolCallEnd { .. })).unwrap();
        match end {
            StreamEvent::ToolCallEnd { input, .. } => assert_eq!(input, r#"{"path":"a.rs"}"#),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_missing_index_is_treated_as_zero() {
        // Some providers omit it when there is exactly one call.
        let events = decode_all(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"tc_1","function":{"name":"now","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert!(events.iter().any(|e| matches!(e, StreamEvent::ToolCallEnd { .. })));
    }

    #[test]
    fn parallel_tool_calls_come_out_in_index_order() {
        let events = decode_all(&[
            r#"{"choices":[{"delta":{"tool_calls":[
                {"index":1,"id":"b","function":{"name":"second","arguments":"{}"}},
                {"index":0,"id":"a","function":{"name":"first","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallEnd { id, .. } => Some(id.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn both_reasoning_field_names_are_understood() {
        for field in ["reasoning_content", "reasoning"] {
            let events = decode_all(&[&format!(r#"{{"choices":[{{"delta":{{"{field}":"hmm"}}}}]}}"#)]);
            assert!(
                events.contains(&StreamEvent::ReasoningDelta("hmm".into())),
                "{field} was not understood"
            );
        }
    }

    #[test]
    fn reasoning_closes_when_prose_begins() {
        let events = decode_all(&[
            r#"{"choices":[{"delta":{"reasoning":"thinking"}}]}"#,
            r#"{"choices":[{"delta":{"content":"answer"}}]}"#,
        ]);
        let reasoning_end = events.iter().position(|e| *e == StreamEvent::ReasoningEnd).unwrap();
        let text_start = events.iter().position(|e| *e == StreamEvent::TextStart).unwrap();
        assert!(reasoning_end < text_start);
    }

    #[test]
    fn a_usage_only_chunk_is_not_treated_as_malformed() {
        // Empty `choices` is how usage arrives; rejecting it loses cost tracking.
        let events = decode_all(&[
            r#"{"choices":[{"delta":{"content":"x"}}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":8}}}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        match events.last().unwrap() {
            StreamEvent::StepFinish { usage, .. } => {
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.cached_input_tokens, 8);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn an_error_payload_is_an_error() {
        let mut state = State::default();
        let result = decode(
            &mut state,
            &crate::sse::Event { name: None, data: r#"{"error":{"message":"bad key"}}"#.into() },
        );
        assert!(result.unwrap_err().to_string().contains("bad key"));
    }
    #[test]
    fn thinking_is_sent_as_the_flat_reasoning_effort() {
        // Verified against OpenRouter as well as OpenAI, so one form covers both.
        let mut request = request();
        request.thinking = crate::thinking::Thinking::Low;
        let body = build(&model(ApiType::OpenAiCompletion), &request);
        assert_eq!(body["reasoning_effort"], "low");
    }

    #[test]
    fn auto_sends_no_effort_field() {
        // Grok rejects the field outright, so it must be omittable.
        let body = build(&model(ApiType::OpenAiCompletion), &request());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn the_models_configured_level_applies_when_the_request_says_auto() {
        let mut model = model(ApiType::OpenAiCompletion);
        model.thinking = crate::thinking::Thinking::High;
        let body = build(&model, &request());
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn the_request_overrides_the_models_default() {
        let mut model = model(ApiType::OpenAiCompletion);
        model.thinking = crate::thinking::Thinking::High;
        let mut request = request();
        request.thinking = crate::thinking::Thinking::Low;
        assert_eq!(build(&model, &request)["reasoning_effort"], "low");
    }

}
