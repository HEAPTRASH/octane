//! Google Generative AI.
//!
//! Gemini and Vertex share this format entirely and differ only in URL and auth,
//! which is why one codec serves both.
//!
//! The awkward part: Google does not stream tool calls. A `functionCall` arrives
//! whole in a single chunk, so a caller cannot dispatch a tool while arguments
//! are still arriving — the decoder emits `ToolCallStart` and `ToolCallEnd`
//! back to back.

use octane_protocol::{Part, Role, ToolCallId, Usage};
use serde_json::json;

use crate::config::ResolvedModel;
use crate::model::ModelRequest;
use crate::stream::{FinishReason, StreamEvent};
use crate::ProviderError;

#[derive(Debug, Default)]
pub struct State {
    started: bool,
    text_open: bool,
    /// Google reuses one id space per response; a counter keeps calls distinct
    /// because the payload carries no id at all.
    call_index: u64,
}

pub fn build(model: &ResolvedModel, request: &ModelRequest) -> serde_json::Value {
    let mut contents: Vec<serde_json::Value> = Vec::new();
    let mut system_parts: Vec<serde_json::Value> = Vec::new();

    for message in &request.messages {
        if message.role == Role::System {
            system_parts.push(json!({ "text": message.text_content() }));
            continue;
        }

        let parts: Vec<serde_json::Value> = message
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::Text { text } | Part::Synthetic { text } if !text.trim().is_empty() => {
                    Some(json!({ "text": text }))
                }
                Part::ToolCall(call) => Some(json!({
                    "functionCall": {
                        "name": call.name,
                        "args": serde_json::from_str::<serde_json::Value>(&call.input)
                            .unwrap_or_else(|_| json!({})),
                    }
                })),
                Part::ToolResult(result) => Some(json!({
                    "functionResponse": {
                        "name": result.call_id.as_str(),
                        "response": { "output": result.output },
                    }
                })),
                Part::Image { media_type, data } => Some(json!({
                    "inlineData": { "mimeType": media_type, "data": data }
                })),
                _ => None,
            })
            .collect();

        if parts.is_empty() {
            continue;
        }
        contents.push(json!({
            // Google has no assistant role; it is "model", and there is no
            // developer or system role in `contents` at all.
            "role": if message.role == Role::Assistant { "model" } else { "user" },
            "parts": parts,
        }));
    }

    let mut body = json!({
        "contents": contents,
        "generationConfig": {
            "maxOutputTokens": request.max_output_tokens.min(model.max_output),
        },
    });

    // Carried in the body for parity with the others, even though Google puts
    // the model in the URL — a proxy may read it, and it costs nothing.
    body["model"] = json!(model.model_id);

    if !system_parts.is_empty() {
        body["systemInstruction"] = json!({ "parts": system_parts });
    }
    if !request.tools.is_empty() {
        body["tools"] = json!([{ "functionDeclarations": super::tool_schemas(model, request) }]);
    }
    if let Some(temperature) = request.temperature.or(model.temperature) {
        body["generationConfig"]["temperature"] = json!(temperature);
    }

    // Google takes a budget, where 0 disables and -1 means "decide for me".
    // `includeThoughts` is separate: a model can think without returning it, so
    // both have to be set or reasoning is spent and then discarded.
    let thinking = if request.thinking.is_auto() { model.thinking } else { request.thinking };
    if let Some(budget) = thinking.budget() {
        body["generationConfig"]["thinkingConfig"] = json!({
            "thinkingBudget": budget,
            "includeThoughts": budget > 0,
        });
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

    let candidate = &value["candidates"][0];

    for part in candidate["content"]["parts"].as_array().into_iter().flatten() {
        if let Some(text) = part["text"].as_str().filter(|t| !t.is_empty()) {
            // `thought: true` marks reasoning rather than answer text.
            if part["thought"].as_bool().unwrap_or(false) {
                events.push(StreamEvent::ReasoningStart);
                events.push(StreamEvent::ReasoningDelta(text.to_string()));
                events.push(StreamEvent::ReasoningEnd);
                continue;
            }
            if !state.text_open {
                state.text_open = true;
                events.push(StreamEvent::TextStart);
            }
            events.push(StreamEvent::TextDelta(text.to_string()));
        }

        if let Some(call) = part.get("functionCall").filter(|c| c.is_object()) {
            // Whole in one chunk, so start/end are emitted together. There is no
            // id in the payload, hence the counter.
            state.call_index += 1;
            let id = ToolCallId::from(format!("call_{}", state.call_index));
            let name = call["name"].as_str().unwrap_or_default().to_string();
            let input = call
                .get("args")
                .map(|args| args.to_string())
                .unwrap_or_else(|| "{}".to_string());

            events.push(StreamEvent::ToolCallStart { id: id.clone(), name });
            events.push(StreamEvent::ToolCallEnd { id, input });
        }
    }

    if let Some(reason) = candidate["finishReason"].as_str() {
        if state.text_open {
            state.text_open = false;
            events.push(StreamEvent::TextEnd);
        }

        let has_tool_call = candidate["content"]["parts"]
            .as_array()
            .is_some_and(|parts| parts.iter().any(|p| p.get("functionCall").is_some()));

        events.push(StreamEvent::StepFinish {
            reason: if has_tool_call { FinishReason::ToolCalls } else { finish_reason(reason) },
            usage: read_usage(&value["usageMetadata"]),
        });
    }

    Ok(events)
}

fn read_usage(usage: &serde_json::Value) -> Usage {
    let read = |key: &str| usage[key].as_u64().unwrap_or_default();
    Usage {
        input_tokens: read("promptTokenCount"),
        output_tokens: read("candidatesTokenCount"),
        cached_input_tokens: read("cachedContentTokenCount"),
        reasoning_tokens: read("thoughtsTokenCount"),
        cost: 0.0,
    }
}

fn finish_reason(reason: &str) -> FinishReason {
    match reason {
        "MAX_TOKENS" => FinishReason::Length,
        "SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" => FinishReason::ContentFilter,
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
    fn system_content_becomes_a_system_instruction() {
        let body = build(&model(ApiType::Google), &request());
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be helpful");
        assert_eq!(body["contents"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_assistant_role_is_called_model() {
        let mut request = request();
        request.messages.push(octane_protocol::Message::text(Role::Assistant, "hi"));
        let body = build(&model(ApiType::Google), &request);
        let roles: Vec<&str> =
            body["contents"].as_array().unwrap().iter().map(|c| c["role"].as_str().unwrap()).collect();
        assert!(roles.contains(&"model"));
        assert!(!roles.contains(&"assistant"));
    }

    #[test]
    fn tools_are_nested_under_function_declarations() {
        let body = build(&model(ApiType::Google), &request());
        assert_eq!(body["tools"][0]["functionDeclarations"][0]["name"], "read");
    }

    #[test]
    fn text_deltas_decode() {
        let events = decode_all(&[r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#]);
        assert_eq!(events[0], StreamEvent::StepStart);
        assert_eq!(events[1], StreamEvent::TextStart);
        assert_eq!(events[2], StreamEvent::TextDelta("hi".into()));
    }

    #[test]
    fn a_function_call_arrives_whole_so_start_and_end_are_emitted_together() {
        // Google does not stream tool calls; this is the consequence.
        let events = decode_all(&[
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{"path":"a.rs"}}}]}}]}"#,
        ]);
        assert!(matches!(events[1], StreamEvent::ToolCallStart { .. }));
        match &events[2] {
            StreamEvent::ToolCallEnd { input, .. } => assert!(input.contains("a.rs")),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parallel_calls_get_distinct_ids() {
        // The payload carries none, so a counter is the only way to tell them
        // apart — and a result cannot be matched to a call without it.
        let events = decode_all(&[
            r#"{"candidates":[{"content":{"parts":[
                {"functionCall":{"name":"a","args":{}}},
                {"functionCall":{"name":"b","args":{}}}]}}]}"#,
        ]);
        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallStart { id, .. } => Some(id.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn thought_parts_decode_as_reasoning_not_prose() {
        let events =
            decode_all(&[r#"{"candidates":[{"content":{"parts":[{"text":"hmm","thought":true}]}}]}"#]);
        assert!(events.contains(&StreamEvent::ReasoningDelta("hmm".into())));
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::TextDelta(_))));
    }

    #[test]
    fn a_turn_containing_a_call_finishes_as_a_tool_turn() {
        let events = decode_all(&[
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"a","args":{}}}]},"finishReason":"STOP"}],"usageMetadata":{}}"#,
        ]);
        match events.last().unwrap() {
            StreamEvent::StepFinish { reason, .. } => assert_eq!(*reason, FinishReason::ToolCalls),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn safety_stops_are_reported_as_content_filtering() {
        let events = decode_all(&[
            r#"{"candidates":[{"content":{"parts":[]},"finishReason":"SAFETY"}],"usageMetadata":{}}"#,
        ]);
        match events.last().unwrap() {
            StreamEvent::StepFinish { reason, .. } => {
                assert_eq!(*reason, FinishReason::ContentFilter)
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn usage_maps_googles_field_names() {
        let events = decode_all(&[
            r#"{"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"cachedContentTokenCount":8,"thoughtsTokenCount":3}}"#,
        ]);
        match events.last().unwrap() {
            StreamEvent::StepFinish { usage, .. } => {
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.output_tokens, 5);
                assert_eq!(usage.cached_input_tokens, 8);
                assert_eq!(usage.reasoning_tokens, 3);
            }
            other => panic!("got {other:?}"),
        }
    }
    #[test]
    fn thinking_is_sent_as_a_budget_with_thoughts_included() {
        let mut request = request();
        request.thinking = crate::thinking::Thinking::Low;
        let body = build(&model(ApiType::Google), &request);

        let config = &body["generationConfig"]["thinkingConfig"];
        assert!(config["thinkingBudget"].as_u64().unwrap() > 0);
        // Without this the model thinks and the reasoning is discarded.
        assert_eq!(config["includeThoughts"], true);
    }

    #[test]
    fn a_zero_budget_also_stops_asking_for_thoughts() {
        let mut request = request();
        request.thinking = crate::thinking::Thinking::Off;
        let body = build(&model(ApiType::Google), &request);

        let config = &body["generationConfig"]["thinkingConfig"];
        assert_eq!(config["thinkingBudget"], 0);
        assert_eq!(config["includeThoughts"], false);
    }

    #[test]
    fn auto_sends_no_thinking_config() {
        let body = build(&model(ApiType::Google), &request());
        assert!(body["generationConfig"].get("thinkingConfig").is_none());
    }

}
