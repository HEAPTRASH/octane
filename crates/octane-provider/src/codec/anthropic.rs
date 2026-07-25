//! Anthropic Messages.
//!
//! The most stateful of the four. Content arrives as indexed blocks —
//! `content_block_start`, a run of `content_block_delta`, `content_block_stop` —
//! and the deltas do not repeat what kind of block they belong to. A decoder has
//! to remember which index is text, which is thinking, and which is a tool call,
//! or it cannot tell a tool's arguments from the assistant's prose.

use std::collections::HashMap;

use octane_protocol::{Part, Role, ToolCallId, Usage};
use serde_json::json;

use crate::config::ResolvedModel;
use crate::model::ModelRequest;
use crate::stream::{FinishReason, StreamEvent};
use crate::ProviderError;

/// What a content block index turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Block {
    Text,
    Thinking,
    ToolCall { id: ToolCallId, name: String, input: String },
}

#[derive(Debug, Default)]
pub struct State {
    blocks: HashMap<u64, Block>,
    usage: Usage,
    /// Set by `message_delta`, emitted at `message_stop`.
    finish: Option<FinishReason>,
}

pub fn build(model: &ResolvedModel, request: &ModelRequest) -> serde_json::Value {
    // The system prompt is a top-level field here, not a message. Leaving it in
    // `messages` is silently accepted by some proxies and rejected by Anthropic.
    let system: Vec<serde_json::Value> = request
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| json!({ "type": "text", "text": m.text_content() }))
        .collect();

    let messages: Vec<serde_json::Value> = request
        .messages
        .iter()
        .filter(|m| m.role != Role::System)
        .filter_map(|message| {
            let content = encode_content(message);
            // Anthropic rejects a message with empty content outright, which is
            // easy to produce when a turn is all tool traffic.
            (!content.is_empty()).then(|| {
                json!({
                    "role": match message.role {
                        Role::Assistant => "assistant",
                        // Developer messages have no Anthropic equivalent, so
                        // they ride as user turns.
                        _ => "user",
                    },
                    "content": content,
                })
            })
        })
        .collect();

    let mut body = json!({
        "model": model.model_id,
        "max_tokens": request.max_output_tokens.min(model.max_output),
        "messages": messages,
        "stream": true,
    });

    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if !request.tools.is_empty() {
        body["tools"] = json!(super::tool_schemas(model, request));
    }
    if let Some(temperature) = request.temperature.or(model.temperature) {
        body["temperature"] = json!(temperature);
    }
    if let Some(top_p) = request.top_p {
        body["top_p"] = json!(top_p);
    }

    // Anthropic caches only what is marked, unlike OpenAI which caches prefixes
    // on its own. The breakpoint goes on the last system block: everything
    // before it is the stable prefix.
    if model.explicit_cache_control {
        if let Some(last) = body["system"].as_array_mut().and_then(|blocks| blocks.last_mut()) {
            last["cache_control"] = json!({ "type": "ephemeral" });
        }
    }

    body
}

fn encode_content(message: &octane_protocol::Message) -> Vec<serde_json::Value> {
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Text { text } | Part::Synthetic { text } => {
                (!text.trim().is_empty()).then(|| json!({ "type": "text", "text": text }))
            }
            Part::Reasoning { .. } => None,
            Part::ToolCall(call) => Some(json!({
                "type": "tool_use",
                "id": call.id.as_str(),
                "name": call.name,
                // Anthropic wants parsed arguments, not the JSON string the
                // model emitted. An unparseable one becomes an empty object
                // rather than failing the whole request.
                "input": serde_json::from_str::<serde_json::Value>(&call.input)
                    .unwrap_or_else(|_| json!({})),
            })),
            Part::ToolResult(result) => Some(json!({
                "type": "tool_result",
                "tool_use_id": result.call_id.as_str(),
                "content": result.output,
                "is_error": result.is_error,
            })),
            Part::File { path, content } => {
                Some(json!({ "type": "text", "text": format!("<file path=\"{path}\">\n{content}\n</file>") }))
            }
            Part::Image { media_type, data } => Some(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data },
            })),
        })
        .collect()
}

pub fn decode(state: &mut State, event: &crate::sse::Event) -> Result<Vec<StreamEvent>, ProviderError> {
    let value: serde_json::Value = serde_json::from_str(&event.data)
        .map_err(|error| ProviderError::Transport(format!("malformed event: {error}")))?;

    // Anthropic puts the type in both the SSE `event:` field and the payload.
    // The payload is authoritative; some proxies drop the header.
    let kind = value["type"].as_str().or(event.name.as_deref()).unwrap_or_default();

    Ok(match kind {
        "message_start" => {
            state.usage = read_usage(&value["message"]["usage"]);
            vec![StreamEvent::StepStart]
        }

        "content_block_start" => {
            let index = value["index"].as_u64().unwrap_or_default();
            let block = &value["content_block"];

            match block["type"].as_str().unwrap_or_default() {
                "tool_use" => {
                    let id = ToolCallId::from(
                        block["id"].as_str().unwrap_or_default().to_string(),
                    );
                    let name = block["name"].as_str().unwrap_or_default().to_string();
                    state.blocks.insert(
                        index,
                        Block::ToolCall { id: id.clone(), name: name.clone(), input: String::new() },
                    );
                    vec![StreamEvent::ToolCallStart { id, name }]
                }
                "thinking" | "redacted_thinking" => {
                    state.blocks.insert(index, Block::Thinking);
                    vec![StreamEvent::ReasoningStart]
                }
                _ => {
                    state.blocks.insert(index, Block::Text);
                    vec![StreamEvent::TextStart]
                }
            }
        }

        "content_block_delta" => {
            let index = value["index"].as_u64().unwrap_or_default();
            let delta = &value["delta"];

            match delta["type"].as_str().unwrap_or_default() {
                "text_delta" => vec![StreamEvent::TextDelta(
                    delta["text"].as_str().unwrap_or_default().to_string(),
                )],
                "thinking_delta" => vec![StreamEvent::ReasoningDelta(
                    delta["thinking"].as_str().unwrap_or_default().to_string(),
                )],
                "input_json_delta" => {
                    let fragment = delta["partial_json"].as_str().unwrap_or_default();
                    // Accumulated as well as emitted: the complete arguments are
                    // needed at block_stop, and the deltas are the only source.
                    let Some(Block::ToolCall { id, input, .. }) = state.blocks.get_mut(&index)
                    else {
                        return Ok(Vec::new());
                    };
                    input.push_str(fragment);
                    vec![StreamEvent::ToolCallInputDelta {
                        id: id.clone(),
                        delta: fragment.to_string(),
                    }]
                }
                _ => Vec::new(),
            }
        }

        "content_block_stop" => {
            let index = value["index"].as_u64().unwrap_or_default();
            match state.blocks.remove(&index) {
                Some(Block::ToolCall { id, input, .. }) => {
                    // An empty-argument tool call streams no deltas at all, so
                    // the input must default to `{}` rather than an empty string
                    // the next parser would reject.
                    let input = if input.trim().is_empty() { "{}".to_string() } else { input };
                    vec![StreamEvent::ToolCallEnd { id, input }]
                }
                Some(Block::Thinking) => vec![StreamEvent::ReasoningEnd],
                Some(Block::Text) => vec![StreamEvent::TextEnd],
                None => Vec::new(),
            }
        }

        "message_delta" => {
            // Output tokens only appear here, at the end.
            if let Some(output) = value["usage"]["output_tokens"].as_u64() {
                state.usage.output_tokens = output;
            }
            state.finish = value["delta"]["stop_reason"].as_str().map(finish_reason);
            Vec::new()
        }

        "message_stop" => {
            vec![StreamEvent::StepFinish {
                reason: state.finish.take().unwrap_or(FinishReason::Stop),
                usage: state.usage,
            }]
        }

        // Errors arrive as events rather than a non-200, so a stream that stops
        // early otherwise looks like a clean finish.
        "error" => {
            let message = value["error"]["message"].as_str().unwrap_or("unknown error");
            return Err(ProviderError::Api { status: 200, message: message.to_string() });
        }

        _ => Vec::new(),
    })
}

fn read_usage(value: &serde_json::Value) -> Usage {
    let read = |key: &str| value[key].as_u64().unwrap_or_default();
    let cached = read("cache_read_input_tokens");
    Usage {
        // Anthropic reports fresh and cached input separately; everything else
        // in octane treats `input_tokens` as the total.
        input_tokens: read("input_tokens") + cached + read("cache_creation_input_tokens"),
        cached_input_tokens: cached,
        output_tokens: read("output_tokens"),
        reasoning_tokens: 0,
        cost: 0.0,
    }
}

fn finish_reason(stop: &str) -> FinishReason {
    match stop {
        "tool_use" => FinishReason::ToolCalls,
        "max_tokens" => FinishReason::Length,
        "refusal" => FinishReason::ContentFilter,
        _ => FinishReason::Stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiType;
    use crate::codec::tests::{model, request};

    fn event(data: &str) -> crate::sse::Event {
        crate::sse::Event { name: None, data: data.to_string() }
    }

    fn decode_all(chunks: &[&str]) -> Vec<StreamEvent> {
        let mut state = State::default();
        chunks.iter().flat_map(|c| decode(&mut state, &event(c)).unwrap()).collect()
    }

    #[test]
    fn the_system_prompt_is_hoisted_out_of_messages() {
        // Anthropic rejects a system role in `messages`.
        let body = build(&model(ApiType::Anthropic), &request());
        assert_eq!(body["system"][0]["text"], "be helpful");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn empty_messages_are_dropped_rather_than_sent() {
        // Anthropic rejects them outright, and an all-tool turn produces them.
        let mut request = request();
        request.messages.push(octane_protocol::Message::new(Role::Assistant, vec![]));
        let body = build(&model(ApiType::Anthropic), &request);
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_cache_breakpoint_lands_on_the_last_system_block() {
        let mut model = model(ApiType::Anthropic);
        model.explicit_cache_control = true;
        let body = build(&model, &request());
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn no_breakpoint_is_added_when_the_format_caches_on_its_own() {
        let body = build(&model(ApiType::Anthropic), &request());
        assert!(body["system"][0].get("cache_control").is_none());
    }

    #[test]
    fn tool_arguments_are_sent_parsed_not_as_a_string() {
        let mut request = request();
        request.messages.push(octane_protocol::Message::new(
            Role::Assistant,
            vec![Part::ToolCall(octane_protocol::ToolCall {
                id: ToolCallId::from("tc_1".to_string()),
                name: "read".into(),
                input: r#"{"path":"a.rs"}"#.into(),
            })],
        ));
        let body = build(&model(ApiType::Anthropic), &request);
        let content = &body["messages"].as_array().unwrap().last().unwrap()["content"][0];
        assert_eq!(content["input"]["path"], "a.rs");
    }

    #[test]
    fn unparseable_tool_arguments_do_not_fail_the_request() {
        let mut request = request();
        request.messages.push(octane_protocol::Message::new(
            Role::Assistant,
            vec![Part::ToolCall(octane_protocol::ToolCall {
                id: ToolCallId::from("tc_1".to_string()),
                name: "read".into(),
                input: "not json".into(),
            })],
        ));
        let body = build(&model(ApiType::Anthropic), &request);
        let content = &body["messages"].as_array().unwrap().last().unwrap()["content"][0];
        assert_eq!(content["input"], serde_json::json!({}));
    }

    #[test]
    fn text_streams_as_start_deltas_and_end() {
        let events = decode_all(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hel"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
        ]);
        assert_eq!(events[0], StreamEvent::StepStart);
        assert_eq!(events[1], StreamEvent::TextStart);
        assert_eq!(events[2], StreamEvent::TextDelta("hel".into()));
        assert_eq!(events[4], StreamEvent::TextEnd);
    }

    #[test]
    fn a_tool_call_accumulates_its_arguments_across_deltas() {
        let events = decode_all(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tc_1","name":"read"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
        ]);
        match events.last().unwrap() {
            StreamEvent::ToolCallEnd { input, .. } => assert_eq!(input, r#"{"path":"a.rs"}"#),
            other => panic!("expected ToolCallEnd, got {other:?}"),
        }
    }

    #[test]
    fn a_zero_argument_tool_call_ends_with_an_empty_object() {
        // No deltas stream at all, and an empty string is not valid JSON.
        let events = decode_all(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tc_1","name":"now"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
        ]);
        match events.last().unwrap() {
            StreamEvent::ToolCallEnd { input, .. } => assert_eq!(input, "{}"),
            other => panic!("expected ToolCallEnd, got {other:?}"),
        }
    }

    #[test]
    fn interleaved_blocks_are_kept_apart_by_index() {
        // The reason this decoder is stateful: deltas do not say what they are.
        let events = decode_all(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tc_1","name":"read"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        ]);
        assert!(matches!(events[2], StreamEvent::ToolCallInputDelta { .. }));
        assert_eq!(events[3], StreamEvent::TextDelta("hi".into()));
    }

    #[test]
    fn thinking_blocks_decode_as_reasoning() {
        let events = decode_all(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
        ]);
        assert_eq!(events[0], StreamEvent::ReasoningStart);
        assert_eq!(events[1], StreamEvent::ReasoningDelta("hmm".into()));
        assert_eq!(events[2], StreamEvent::ReasoningEnd);
    }

    #[test]
    fn stop_reasons_map_to_finish_reasons() {
        for (stop, expected) in [
            ("tool_use", FinishReason::ToolCalls),
            ("end_turn", FinishReason::Stop),
            ("max_tokens", FinishReason::Length),
        ] {
            let events = decode_all(&[
                &format!(r#"{{"type":"message_delta","delta":{{"stop_reason":"{stop}"}}}}"#),
                r#"{"type":"message_stop"}"#,
            ]);
            match events.last().unwrap() {
                StreamEvent::StepFinish { reason, .. } => assert_eq!(*reason, expected, "{stop}"),
                other => panic!("expected StepFinish, got {other:?}"),
            }
        }
    }

    #[test]
    fn usage_combines_fresh_and_cached_input() {
        let events = decode_all(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":90}}}"#,
            r#"{"type":"message_delta","usage":{"output_tokens":5}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        match events.last().unwrap() {
            StreamEvent::StepFinish { usage, .. } => {
                assert_eq!(usage.input_tokens, 100);
                assert_eq!(usage.cached_input_tokens, 90);
                assert_eq!(usage.output_tokens, 5);
            }
            other => panic!("expected StepFinish, got {other:?}"),
        }
    }

    #[test]
    fn an_error_event_is_an_error_not_a_clean_finish() {
        // These arrive mid-stream with a 200, so ignoring them looks like the
        // model simply stopped.
        let mut state = State::default();
        let result = decode(
            &mut state,
            &event(r#"{"type":"error","error":{"message":"overloaded"}}"#),
        );
        assert!(result.unwrap_err().to_string().contains("overloaded"));
    }

    #[test]
    fn the_payload_type_wins_over_a_missing_sse_header() {
        // Some proxies drop the `event:` line.
        let mut state = State::default();
        let events = decode(
            &mut state,
            &crate::sse::Event { name: None, data: r#"{"type":"message_stop"}"#.into() },
        )
        .unwrap();
        assert!(matches!(events[0], StreamEvent::StepFinish { .. }));
    }
}
