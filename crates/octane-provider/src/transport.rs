//! The HTTP layer.
//!
//! Deliberately thin. Everything format-specific lives in [`crate::codec`] as
//! pure functions, so this only builds a request, applies auth, and turns a byte
//! stream into [`StreamEvent`]s. The rule: nothing here should need a test that
//! requires a network.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use octane_protocol::Message;

use crate::api::ApiType;
use crate::codec::{self, Decoder};
use crate::config::{Auth, ResolvedModel};
use crate::model::{LanguageModel, ModelInfo, ModelRequest, ToolSchema};
use crate::stream::ModelStream;
use crate::{ProviderError, ProviderId};

/// Idle timeout. A stream that goes quiet for this long is treated as dead.
///
/// Providers stall without closing the connection, and without this the agent
/// waits forever with a spinner and no explanation.
pub const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// Connect timeout, which should be short — a hung connect is a broken endpoint.
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// A model reachable over HTTP.
pub struct HttpModel {
    resolved: ResolvedModel,
    info: ModelInfo,
    client: reqwest::Client,
}

impl std::fmt::Debug for HttpModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpModel")
            .field("reference", &self.resolved.reference)
            .field("api", &self.resolved.api)
            .finish_non_exhaustive()
    }
}

impl HttpModel {
    pub fn new(resolved: ResolvedModel) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            // No total timeout: a long agentic turn is legitimately slow, and
            // the idle timeout is what actually distinguishes stalled from busy.
            .user_agent(concat!("octane/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| ProviderError::Transport(error.to_string()))?;

        let info = ModelInfo {
            provider: ProviderId(resolved.provider.clone()),
            id: resolved.model_id.clone(),
            display_name: resolved.display_name.clone(),
            context_window: resolved.context_window,
            max_output_tokens: resolved.max_output,
            supports_tools: true,
            supports_images: resolved.images,
            supports_reasoning: resolved.reasoning,
            needs_explicit_cache_control: resolved.explicit_cache_control,
            pricing: resolved.pricing,
        };

        Ok(Self { resolved, info, client })
    }

    pub fn resolved(&self) -> &ResolvedModel {
        &self.resolved
    }

    /// The URL for one request.
    ///
    /// Google is the odd one: the model and the action live in the path rather
    /// than the body, so the URL is built per request instead of once.
    fn url(&self) -> String {
        match self.resolved.api {
            ApiType::Google => format!(
                "{}/models/{}:streamGenerateContent?alt=sse",
                self.resolved.base_url.trim_end_matches('/'),
                self.resolved.model_id
            ),
            _ => self.resolved.base_url.clone(),
        }
    }

    /// Apply auth to a request builder.
    async fn authorize(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        Ok(match &self.resolved.auth {
            Auth::None => request,

            Auth::ApiKey { value, header, prefix } => {
                request.header(header.as_str(), format!("{prefix}{value}"))
            }

            Auth::TokenFile { path, header, prefix } => {
                let token = tokio::fs::read_to_string(path).await.map_err(|error| {
                    ProviderError::Auth(format!("reading token file {path}: {error}"))
                })?;
                request.header(header.as_str(), format!("{prefix}{}", token.trim()))
            }

            // Deliberately an error rather than an unauthenticated request:
            // a 401 from a silently unsigned call is far harder to diagnose than
            // being told the credential provider is not implemented.
            Auth::GoogleVertex { .. } => {
                return Err(ProviderError::Auth(
                    "Vertex credentials are not implemented yet. Use `tokenFile` with a token \
                     from `gcloud auth print-access-token`."
                        .into(),
                ));
            }
            Auth::AwsSigV4 { .. } => {
                return Err(ProviderError::Auth(
                    "Bedrock SigV4 signing is not implemented yet.".into(),
                ));
            }
        })
    }
}

#[async_trait]
impl LanguageModel for HttpModel {
    fn info(&self) -> &ModelInfo {
        &self.info
    }

    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ProviderError> {
        let body = codec::build_request(&self.resolved, &request);

        let mut builder = self
            .client
            .post(self.url())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream");

        for (name, value) in &self.resolved.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder = self.authorize(builder).await?;

        let response = builder
            .json(&body)
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(classify_error(status.as_u16(), &text));
        }

        let api = self.resolved.api;
        let mut decoder = Decoder::new(api);
        let mut parser = crate::sse::Parser::new();
        let mut bytes = response.bytes_stream();

        let stream = async_stream::stream! {
            loop {
                // A provider that stalls without closing would otherwise hang
                // the turn indefinitely.
                let next = tokio::time::timeout(IDLE_TIMEOUT, bytes.next()).await;

                let chunk = match next {
                    Err(_) => {
                        yield Err(ProviderError::Transport(format!(
                            "no data for {}s; the provider stalled",
                            IDLE_TIMEOUT.as_secs()
                        )));
                        return;
                    }
                    Ok(None) => break,
                    Ok(Some(Err(error))) => {
                        yield Err(ProviderError::Transport(error.to_string()));
                        return;
                    }
                    Ok(Some(Ok(chunk))) => chunk,
                };

                for event in parser.push(&chunk) {
                    match decoder.decode(&event) {
                        Ok(decoded) => for item in decoded { yield Ok(item); },
                        Err(error) => { yield Err(error); return; }
                    }
                }
            }

            // A final event without its trailing blank line is common; dropping
            // it loses the last token of the response.
            if let Some(event) = parser.finish() {
                if let Ok(decoded) = decoder.decode(&event) {
                    for item in decoded { yield Ok(item); }
                }
            }
        };

        Ok(Box::pin(stream))
    }

    fn estimate_tokens(&self, messages: &[Message]) -> u64 {
        // ~4 bytes per token. Crude by design: exact counts come back in usage,
        // and this only has to be good enough to decide when to compact.
        let bytes: usize = messages
            .iter()
            .flat_map(|message| &message.parts)
            .map(|part| match part {
                octane_protocol::Part::Text { text }
                | octane_protocol::Part::Reasoning { text }
                | octane_protocol::Part::Synthetic { text } => text.len(),
                octane_protocol::Part::ToolCall(call) => call.name.len() + call.input.len(),
                octane_protocol::Part::ToolResult(result) => result.output.len(),
                octane_protocol::Part::File { path, content } => path.len() + content.len(),
                octane_protocol::Part::Image { .. } => 4_000,
            })
            .sum();
        (bytes / 4) as u64
    }
}

/// Turn an HTTP failure into something actionable.
///
/// The status alone is not enough: 401 and 429 both mean "your fault" but
/// require completely different responses, and a provider's message usually says
/// which key or which limit.
pub fn classify_error(status: u16, body: &str) -> ProviderError {
    let message = extract_message(body).unwrap_or_else(|| truncate(body, 300));

    match status {
        401 | 403 => ProviderError::Auth(message),
        429 => ProviderError::RateLimited {
            // Retry-After would be better, but it is not in the body and the
            // caller has the headers only on the error path.
            retry_after_secs: None,
        },
        // Anthropic and OpenAI both use 400 for an over-long prompt, so the body
        // is the only way to tell it from an ordinary bad request. Getting this
        // right is what lets the loop compact and retry instead of failing.
        400 if is_context_overflow(&message) => {
            ProviderError::ContextWindowExceeded { used: 0, limit: 0 }
        }
        _ => ProviderError::Api { status, message },
    }
}

fn is_context_overflow(message: &str) -> bool {
    let lower = message.to_lowercase();
    ["context length", "context_length", "too long", "maximum context", "prompt is too long"]
        .iter()
        .any(|needle| lower.contains(needle))
}

/// Pull the human-readable message out of whichever error envelope this is.
fn extract_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;

    // Each path is tried independently. Using `?` inside the loop would abandon
    // the whole search at the first envelope that does not match, so only the
    // first candidate would ever be reachable.
    [
        &["error", "message"][..],
        &["error", "msg"][..],
        &["message"][..],
        &["detail"][..],
    ]
    .into_iter()
    .find_map(|path| {
        path.iter()
            .try_fold(&value, |current, key| current.get(*key))
            .and_then(|found| found.as_str())
            .map(ToString::to_string)
    })
}

fn truncate(text: &str, limit: usize) -> String {
    let cleaned = text.trim();
    if cleaned.chars().count() <= limit {
        return cleaned.to_string();
    }
    cleaned.chars().take(limit).collect::<String>() + "…"
}

/// Build a [`LanguageModel`] for a resolved configuration.
pub fn connect(resolved: ResolvedModel) -> Result<Arc<dyn LanguageModel>, ProviderError> {
    Ok(Arc::new(HttpModel::new(resolved)?))
}

/// A request shaped for a model, for callers that only have messages and tools.
pub fn request_for(
    model: &ModelInfo,
    messages: Vec<Message>,
    tools: Vec<ToolSchema>,
) -> ModelRequest {
    ModelRequest {
        messages,
        tools,
        max_output_tokens: model.max_output_tokens,
        temperature: None,
        top_p: None,
        thinking: crate::thinking::Thinking::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Auth;

    fn model(api: ApiType, base_url: &str) -> ResolvedModel {
        ResolvedModel {
            provider: "p".into(),
            key: "m".into(),
            reference: "p/m".into(),
            display_name: "M".into(),
            model_id: "the-model".into(),
            api,
            base_url: base_url.into(),
            auth: Auth::None,
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

    #[test]
    fn most_formats_post_to_the_configured_url() {
        for api in [ApiType::Anthropic, ApiType::OpenAiCompletion, ApiType::OpenAiResponses] {
            let http = HttpModel::new(model(api, "https://x/v1/messages")).unwrap();
            assert_eq!(http.url(), "https://x/v1/messages");
        }
    }

    #[test]
    fn google_builds_the_model_and_action_into_the_path() {
        let http = HttpModel::new(model(ApiType::Google, "https://x/v1beta")).unwrap();
        assert_eq!(
            http.url(),
            "https://x/v1beta/models/the-model:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn google_urls_survive_a_trailing_slash() {
        let http = HttpModel::new(model(ApiType::Google, "https://x/v1beta/")).unwrap();
        assert!(!http.url().contains("//models"));
    }

    #[test]
    fn auth_failures_are_distinguished_from_other_errors() {
        // 401 and 429 both mean "your fault" and need opposite responses.
        assert!(matches!(
            classify_error(401, r#"{"error":{"message":"invalid key"}}"#),
            ProviderError::Auth(_)
        ));
        assert!(matches!(
            classify_error(429, "{}"),
            ProviderError::RateLimited { .. }
        ));
        assert!(matches!(
            classify_error(500, "boom"),
            ProviderError::Api { status: 500, .. }
        ));
    }

    #[test]
    fn an_over_long_prompt_is_recognized_despite_arriving_as_a_400() {
        // This is what lets the loop compact and retry instead of failing.
        for body in [
            r#"{"error":{"message":"prompt is too long: 210000 tokens > 200000"}}"#,
            r#"{"error":{"message":"This model's maximum context length is 128000 tokens"}}"#,
        ] {
            assert!(
                matches!(classify_error(400, body), ProviderError::ContextWindowExceeded { .. }),
                "not recognized: {body}"
            );
        }
    }

    #[test]
    fn an_ordinary_400_stays_an_api_error() {
        assert!(matches!(
            classify_error(400, r#"{"error":{"message":"unknown parameter 'foo'"}}"#),
            ProviderError::Api { status: 400, .. }
        ));
    }

    #[test]
    fn the_message_is_pulled_from_whichever_envelope_the_provider_uses() {
        for body in [
            r#"{"error":{"message":"nope"}}"#,
            r#"{"message":"nope"}"#,
            r#"{"detail":"nope"}"#,
        ] {
            assert_eq!(extract_message(body).as_deref(), Some("nope"), "{body}");
        }
    }

    #[test]
    fn a_non_json_error_body_is_kept_as_text() {
        // Gateways return HTML error pages, and a parse failure must not hide
        // the only clue about what went wrong.
        let error = classify_error(502, "<html><body>Bad Gateway</body></html>");
        assert!(error.to_string().contains("Bad Gateway"));
    }

    #[test]
    fn a_huge_error_body_is_truncated() {
        let error = classify_error(500, &"x".repeat(10_000));
        assert!(error.to_string().len() < 500);
    }

    #[tokio::test]
    async fn unimplemented_credentials_error_rather_than_sending_an_unsigned_request() {
        // A 401 from a silently unsigned call is far harder to diagnose.
        let mut resolved = model(ApiType::Google, "https://x/v1beta");
        resolved.auth = Auth::GoogleVertex {
            project: "p".into(),
            location: "us-central1".into(),
            credentials: None,
        };
        let http = HttpModel::new(resolved).unwrap();

        let error = http
            .authorize(http.client.post("https://x"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("not implemented"));
        // ...and points at the workaround that does exist.
        assert!(error.contains("tokenFile"));
    }

    #[tokio::test]
    async fn a_missing_token_file_names_the_path() {
        let mut resolved = model(ApiType::Anthropic, "https://x/v1/messages");
        resolved.auth = Auth::TokenFile {
            path: "/nonexistent/token".into(),
            header: "Authorization".into(),
            prefix: "Bearer ".into(),
        };
        let http = HttpModel::new(resolved).unwrap();

        let error = http.authorize(http.client.post("https://x")).await.unwrap_err().to_string();
        assert!(error.contains("/nonexistent/token"));
    }
}
