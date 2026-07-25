//! OpenAI Responses.
//!
//! Named events rather than opaque chunks, which makes the decoder simpler than
//! Completions'. Two differences that matter: `input` replaces `messages`, and
//! reasoning arrives as its own typed item rather than a field smuggled into a
//! delta.

use octane_protocol::{Part, Role, ToolCallId, Usage};
use serde_json::json;

use crate::config::ResolvedModel;
use crate::model::ModelRequest;
use crate::stream::{FinishReason, StreamEvent};
use crate::ProviderError;

#[derive(Debug, Default)]
pub struct State {
    usage: Usage,
    finish: Option<FinishReason>,
}

pub fn build(model: &ResolvedModel, request: &ModelRequest) -> serde_json::Value {
    let mut input: Vec<serde_json::Value> = Vec::new();
    let mut instructions = String::new();

    for message in &request.messages {
        // System content becomes `instructions`, which is what the format
        // caches most aggressively.
        if message.role == Role::System {
            if !instructions.is_empty() {
                instructions.push_str("\n\n");
            }
            instructions.push_str(&message.text_content());
            continue;
        }

        for part in &message.parts {
            match part {
                Part::ToolCall(call) => input.push(json!({
                    "type": "function_call",
                    "call_id": call.id.as_str(),
                    "name": call.name,
                    "arguments": call.input,
                })),
                Part::ToolResult(result) => input.push(json!({
                    "type": "function_call_output",
                    "call_id": result.call_id.as_str(),
                    "output": result.output,
                })),
                Part::Text { text } | Part::Synthetic { text } if !text.trim().is_empty() => {
                    input.push(json!({
                        "role": match message.role {
                            Role::Assistant => "assistant",
                            Role::Developer => "developer",
                            _ => "user",
                        },
                        "content": text,
                    }))
                }
                _ => {}
            }
        }
    }

    let mut body = json!({
        "model": model.model_id,
        "input": input,
        "stream": true,
        "max_output_tokens": request.max_output_tokens.min(model.max_output),
        // Codex does not use this and neither do we: it would require the
        // provider to hold conversation state, which rules out zero-retention.
        "store": false,
    });

    if !instructions.is_empty() {
        body["instructions"] = json!(instructions);
    }
    if !request.tools.is_empty() {
        body["tools"] = json!(super::tool_schemas(model, request));
    }
    if let Some(temperature) = request.temperature.or(model.temperature) {
        body["temperature"] = json!(temperature);
    }

    // Nested under `reasoning` here, unlike the flat field Completions takes.
    let thinking = if request.thinking.is_auto() { model.thinking } else { request.thinking };
    if let Some(effort) = thinking.effort() {
        body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
    }
    body
}

pub fn decode(state: &mut State, event: &crate::sse::Event) -> Result<Vec<StreamEvent>, ProviderError> {
    let value: serde_json::Value = serde_json::from_str(&event.data)
        .map_err(|error| ProviderError::Transport(format!("malformed event: {error}")))?;

    let kind = value["type"].as_str().or(event.name.as_deref()).unwrap_or_default();

    Ok(match kind {
        "response.created" => vec![StreamEvent::StepStart],

        "response.output_item.added" => match value["item"]["type"].as_str() {
            Some("function_call") => vec![StreamEvent::ToolCallStart {
                id: ToolCallId::from(
                    value["item"]["call_id"].as_str().unwrap_or_default().to_string(),
                ),
                name: value["item"]["name"].as_str().unwrap_or_default().to_string(),
            }],
            Some("reasoning") => vec![StreamEvent::ReasoningStart],
            _ => Vec::new(),
        },

        "response.output_text.delta" => {
            vec![StreamEvent::TextDelta(value["delta"].as_str().unwrap_or_default().to_string())]
        }
        "response.output_text.done" => vec![StreamEvent::TextEnd],

        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            vec![StreamEvent::ReasoningDelta(
                value["delta"].as_str().unwrap_or_default().to_string(),
            )]
        }

        "response.function_call_arguments.delta" => vec![StreamEvent::ToolCallInputDelta {
            id: ToolCallId::from(value["item_id"].as_str().unwrap_or_default().to_string()),
            delta: value["delta"].as_str().unwrap_or_default().to_string(),
        }],

        "response.function_call_arguments.done" => {
            let arguments = value["arguments"].as_str().unwrap_or_default();
            vec![StreamEvent::ToolCallEnd {
                id: ToolCallId::from(value["item_id"].as_str().unwrap_or_default().to_string()),
                // Same reasoning as elsewhere: an empty string is not JSON.
                input: if arguments.trim().is_empty() { "{}".into() } else { arguments.into() },
            }]
        }

        "response.output_item.done" if value["item"]["type"] == "reasoning" => {
            vec![StreamEvent::ReasoningEnd]
        }

        "response.completed" | "response.incomplete" => {
            let response = &value["response"];
            state.usage = read_usage(&response["usage"]);

            // A response with any function call in its output is a tool turn,
            // whatever the status says.
            let has_tool_call = response["output"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["type"] == "function_call"));

            let reason = if has_tool_call {
                FinishReason::ToolCalls
            } else if response["incomplete_details"]["reason"] == "max_output_tokens" {
                FinishReason::Length
            } else {
                state.finish.take().unwrap_or(FinishReason::Stop)
            };

            vec![StreamEvent::StepFinish { reason, usage: state.usage }]
        }

        "error" | "response.failed" => {
            let message = value["response"]["error"]["message"]
                .as_str()
                .or_else(|| value["message"].as_str())
                .unwrap_or("unknown error");
            return Err(ProviderError::Api { status: 200, message: message.to_string() });
        }

        _ => Vec::new(),
    })
}

fn read_usage(usage: &serde_json::Value) -> Usage {
    Usage {
        input_tokens: usage["input_tokens"].as_u64().unwrap_or_default(),
        output_tokens: usage["output_tokens"].as_u64().unwrap_or_default(),
        cached_input_tokens: usage["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or_default(),
        reasoning_tokens: usage["output_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .unwrap_or_default(),
        cost: 0.0,
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
    fn system_content_becomes_instructions() {
        let body = build(&model(ApiType::OpenAiResponses), &request());
        assert_eq!(body["instructions"], "be helpful");
        // ...and does not also appear in the input.
        let roles: Vec<&str> =
            body["input"].as_array().unwrap().iter().filter_map(|i| i["role"].as_str()).collect();
        assert!(!roles.contains(&"system"));
    }

    #[test]
    fn conversation_state_is_never_stored_server_side() {
        // Storing it would rule out zero-retention deployments.
        let body = build(&model(ApiType::OpenAiResponses), &request());
        assert_eq!(body["store"], false);
    }

    #[test]
    fn tool_calls_and_results_are_typed_input_items() {
        let mut request = request();
        request.messages.push(octane_protocol::Message::new(
            Role::Assistant,
            vec![Part::ToolCall(octane_protocol::ToolCall {
                id: ToolCallId::from("tc_1".to_string()),
                name: "read".into(),
                input: "{}".into(),
            })],
        ));
        request.messages.push(octane_protocol::Message::new(
            Role::User,
            vec![Part::ToolResult(octane_protocol::ToolResult {
                call_id: ToolCallId::from("tc_1".to_string()),
                output: "ok".into(),
                metadata: None,
                is_error: false,
            })],
        ));

        let body = build(&model(ApiType::OpenAiResponses), &request);
        let items = body["input"].as_array().unwrap();
        assert!(items.iter().any(|i| i["type"] == "function_call" && i["call_id"] == "tc_1"));
        assert!(items.iter().any(|i| i["type"] == "function_call_output"));
    }

    #[test]
    fn text_deltas_decode() {
        let events = decode_all(&[
            r#"{"type":"response.created"}"#,
            r#"{"type":"response.output_text.delta","delta":"hi"}"#,
            r#"{"type":"response.output_text.done"}"#,
        ]);
        assert_eq!(events[0], StreamEvent::StepStart);
        assert_eq!(events[1], StreamEvent::TextDelta("hi".into()));
        assert_eq!(events[2], StreamEvent::TextEnd);
    }

    #[test]
    fn a_tool_call_decodes_start_deltas_and_end() {
        let events = decode_all(&[
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"tc_1","name":"read"}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"tc_1","delta":"{\"a\":1}"}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"tc_1","arguments":"{\"a\":1}"}"#,
        ]);
        assert!(matches!(events[0], StreamEvent::ToolCallStart { .. }));
        match &events[2] {
            StreamEvent::ToolCallEnd { input, .. } => assert_eq!(input, r#"{"a":1}"#),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_response_containing_a_function_call_finishes_as_a_tool_turn() {
        let events = decode_all(&[
            r#"{"type":"response.completed","response":{"output":[{"type":"function_call"}],"usage":{"input_tokens":1,"output_tokens":2}}}"#,
        ]);
        match events.last().unwrap() {
            StreamEvent::StepFinish { reason, .. } => assert_eq!(*reason, FinishReason::ToolCalls),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn hitting_the_output_limit_is_reported_as_truncation() {
        let events = decode_all(&[
            r#"{"type":"response.incomplete","response":{"output":[],"incomplete_details":{"reason":"max_output_tokens"},"usage":{}}}"#,
        ]);
        match events.last().unwrap() {
            StreamEvent::StepFinish { reason, .. } => assert_eq!(*reason, FinishReason::Length),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn reasoning_items_decode_as_reasoning() {
        let events = decode_all(&[
            r#"{"type":"response.output_item.added","item":{"type":"reasoning"}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"hmm"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"reasoning"}}"#,
        ]);
        assert_eq!(events[0], StreamEvent::ReasoningStart);
        assert_eq!(events[1], StreamEvent::ReasoningDelta("hmm".into()));
        assert_eq!(events[2], StreamEvent::ReasoningEnd);
    }

    #[test]
    fn usage_includes_cached_and_reasoning_tokens() {
        let events = decode_all(&[
            r#"{"type":"response.completed","response":{"output":[],"usage":{"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":90},"output_tokens_details":{"reasoning_tokens":15}}}}"#,
        ]);
        match events.last().unwrap() {
            StreamEvent::StepFinish { usage, .. } => {
                assert_eq!(usage.cached_input_tokens, 90);
                assert_eq!(usage.reasoning_tokens, 15);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_failure_event_is_an_error() {
        let mut state = State::default();
        let result = decode(
            &mut state,
            &crate::sse::Event {
                name: None,
                data: r#"{"type":"response.failed","response":{"error":{"message":"rate limited"}}}"#.into(),
            },
        );
        assert!(result.unwrap_err().to_string().contains("rate limited"));
    }
    #[test]
    fn thinking_is_nested_under_reasoning() {
        let mut request = request();
        request.thinking = crate::thinking::Thinking::Medium;
        let body = build(&model(ApiType::OpenAiResponses), &request);
        assert_eq!(body["reasoning"]["effort"], "medium");
        // A summary is requested, or the reasoning is spent and never returned.
        assert_eq!(body["reasoning"]["summary"], "auto");
    }

    #[test]
    fn auto_sends_no_reasoning_field() {
        assert!(build(&model(ApiType::OpenAiResponses), &request()).get("reasoning").is_none());
    }

}
