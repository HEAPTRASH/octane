//! Wire-format codecs.
//!
//! Each codec is two pure functions: build a request body from a
//! [`ModelRequest`], and turn one decoded SSE event into zero or more
//! [`StreamEvent`]s. No HTTP, no async, no I/O — so every format quirk below is
//! testable against a literal chunk captured from the wire, which is the only
//! way to be sure about any of them.
//!
//! [`crate::transport`] is the thin part that moves bytes.

pub mod anthropic;
pub mod google;
pub mod openai_completion;
pub mod openai_responses;

use crate::api::ApiType;
use crate::config::ResolvedModel;
use crate::model::ModelRequest;
use crate::stream::StreamEvent;

/// Per-format decoding state.
///
/// Anthropic and the Responses API address content by index, so a decoder has to
/// remember which block is which between events; a stateless `parse(event)`
/// cannot. Completions carries the index in every chunk and needs almost none of
/// this, but shares the type so the transport stays format-agnostic.
#[derive(Debug, Default)]
pub struct Decoder {
    api: Option<ApiType>,
    anthropic: anthropic::State,
    responses: openai_responses::State,
    completion: openai_completion::State,
    google: google::State,
}

impl Decoder {
    pub fn new(api: ApiType) -> Self {
        Self { api: Some(api), ..Default::default() }
    }

    /// Decode one SSE event.
    pub fn decode(&mut self, event: &crate::sse::Event) -> Result<Vec<StreamEvent>, crate::ProviderError> {
        if event.is_done() {
            return Ok(Vec::new());
        }
        match self.api {
            Some(ApiType::Anthropic) => anthropic::decode(&mut self.anthropic, event),
            Some(ApiType::OpenAiCompletion) => {
                openai_completion::decode(&mut self.completion, event)
            }
            Some(ApiType::OpenAiResponses) => openai_responses::decode(&mut self.responses, event),
            Some(ApiType::Google) => google::decode(&mut self.google, event),
            None => Ok(Vec::new()),
        }
    }
}

/// Build the request body for a model.
pub fn build_request(model: &ResolvedModel, request: &ModelRequest) -> serde_json::Value {
    let mut body = match model.api {
        ApiType::Anthropic => anthropic::build(model, request),
        ApiType::OpenAiCompletion => openai_completion::build(model, request),
        ApiType::OpenAiResponses => openai_responses::build(model, request),
        ApiType::Google => google::build(model, request),
    };

    // The provider's `body` merges last, so a proxy's routing metadata wins over
    // anything built here. Junie documents the same precedence, and it is the
    // right way round: the user configured it deliberately.
    if let Some(extra) = &model.body {
        body = crate::config::merge_json(body, extra);
    }
    body
}

/// JSON Schema for a tool, as each format wants it.
pub(crate) fn tool_schemas(model: &ResolvedModel, request: &ModelRequest) -> Vec<serde_json::Value> {
    request
        .tools
        .iter()
        .map(|tool| match model.api {
            // Anthropic nests the schema under `input_schema`.
            ApiType::Anthropic => serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            }),
            // The Responses API flattened tools in 2025: no `function` wrapper.
            ApiType::OpenAiResponses => serde_json::json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            }),
            ApiType::OpenAiCompletion => serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                },
            }),
            ApiType::Google => serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolSchema;
    use octane_protocol::{Message, Role};

    pub(crate) fn model(api: ApiType) -> ResolvedModel {
        ResolvedModel {
            provider: "test".into(),
            key: "m".into(),
            reference: "test/m".into(),
            display_name: "M".into(),
            model_id: "the-model".into(),
            api,
            base_url: "https://example.com/v1".into(),
            auth: crate::config::Auth::None,
            headers: Default::default(),
            body: None,
            context_window: 200_000,
            max_output: 4_096,
            temperature: None,
            pricing: Default::default(),
            reasoning: false,
            images: false,
            explicit_cache_control: false,
            thinking: Default::default(),
        }
    }

    pub(crate) fn request() -> ModelRequest {
        ModelRequest {
            messages: vec![
                Message::text(Role::System, "be helpful"),
                Message::text(Role::User, "hello"),
            ],
            tools: vec![ToolSchema {
                name: "read".into(),
                description: "Reads a file.".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            max_output_tokens: 1_024,
            temperature: None,
            top_p: None,
            thinking: Default::default(),
        }
    }

    #[test]
    fn every_format_produces_a_body_naming_the_model() {
        for api in [
            ApiType::Anthropic,
            ApiType::OpenAiCompletion,
            ApiType::OpenAiResponses,
            ApiType::Google,
        ] {
            let body = build_request(&model(api), &request());
            // Google carries the model in the URL, not the body.
            if api != ApiType::Google {
                assert_eq!(body["model"], "the-model", "{api}");
            }
            assert!(body.is_object(), "{api}");
        }
    }

    #[test]
    fn tool_schemas_match_each_formats_shape() {
        let request = request();

        let anthropic = tool_schemas(&model(ApiType::Anthropic), &request);
        assert!(anthropic[0].get("input_schema").is_some());

        // The Responses API flattened this; the older Completions API did not.
        let responses = tool_schemas(&model(ApiType::OpenAiResponses), &request);
        assert_eq!(responses[0]["name"], "read");
        assert!(responses[0].get("function").is_none());

        let completion = tool_schemas(&model(ApiType::OpenAiCompletion), &request);
        assert_eq!(completion[0]["function"]["name"], "read");
    }

    #[test]
    fn the_configured_body_wins_over_the_built_one() {
        // A proxy's routing metadata is configured deliberately; the codec's
        // defaults are not.
        let mut model = model(ApiType::OpenAiCompletion);
        model.body = Some(serde_json::json!({ "model": "overridden", "tags": ["x"] }));

        let body = build_request(&model, &request());
        assert_eq!(body["model"], "overridden");
        assert_eq!(body["tags"], serde_json::json!(["x"]));
    }

    #[test]
    fn the_done_sentinel_decodes_to_nothing() {
        let mut decoder = Decoder::new(ApiType::OpenAiCompletion);
        let event = crate::sse::Event { name: None, data: "[DONE]".into() };
        assert!(decoder.decode(&event).unwrap().is_empty());
    }
}
