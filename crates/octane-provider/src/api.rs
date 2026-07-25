//! The wire formats worth speaking.
//!
//! Four of them cover essentially every endpoint (`RESEARCH.md` §L) — a finding
//! pi reached independently and Junie shipped verbatim. Everything else is a
//! dialect of one of these, handled by a per-provider transform rather than a
//! fifth format.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ApiType {
    /// `/v1/chat/completions`. The long tail: Ollama, vLLM, LM Studio,
    /// llama.cpp, Groq, Cerebras, xAI, Mistral, OpenRouter, DeepSeek, LiteLLM,
    /// Azure OpenAI, and most corporate gateways.
    #[serde(rename = "openai-completion", alias = "OpenAICompletion")]
    OpenAiCompletion,

    /// `/v1/responses`. Newer OpenAI, and endpoints implementing it.
    #[serde(rename = "openai-responses", alias = "OpenAIResponses")]
    OpenAiResponses,

    /// `/v1/messages`. Anthropic, and Anthropic-compatible gateways.
    #[serde(rename = "anthropic", alias = "Anthropic")]
    Anthropic,

    /// `:streamGenerateContent`. Gemini API and Vertex share this format and
    /// differ only in URL shape and auth, which is why the two are one variant
    /// here and two [`crate::config::Auth`] variants.
    #[serde(rename = "google", alias = "Google")]
    Google,
}

impl ApiType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompletion => "openai-completion",
            Self::OpenAiResponses => "openai-responses",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
        }
    }

    /// Path appended when a `baseUrl` names only a host and version prefix.
    ///
    /// Lets `https://api.deepseek.com/v1` work as written, which is how people
    /// copy it out of documentation, while a full endpoint URL is left alone.
    pub fn default_path(self) -> &'static str {
        match self {
            Self::OpenAiCompletion => "/chat/completions",
            Self::OpenAiResponses => "/responses",
            Self::Anthropic => "/messages",
            // Google puts the model and the action in the path, so it is
            // templated per-request rather than appended here.
            Self::Google => "",
        }
    }

    /// Whether this format streams tool calls.
    ///
    /// Google does not, so a caller must buffer the whole response before it can
    /// dispatch a tool. Encoded here rather than discovered as a mystery bug.
    pub fn streams_tool_calls(self) -> bool {
        !matches!(self, Self::Google)
    }
}

impl std::fmt::Display for ApiType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ApiType {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().replace('_', "-").as_str() {
            "openai-completion" | "openai" | "openai-compat" | "openaicompletion" => {
                Ok(Self::OpenAiCompletion)
            }
            "openai-responses" | "responses" | "openairesponses" => Ok(Self::OpenAiResponses),
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "google" | "gemini" | "google-vertex" | "vertex" | "vertexai" => Ok(Self::Google),
            other => Err(format!("unknown api type {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junie_spellings_are_accepted() {
        // Profiles are portable between the two, so their casing must load.
        for (text, expected) in [
            (r#""OpenAICompletion""#, ApiType::OpenAiCompletion),
            (r#""OpenAIResponses""#, ApiType::OpenAiResponses),
            (r#""Anthropic""#, ApiType::Anthropic),
            (r#""Google""#, ApiType::Google),
        ] {
            assert_eq!(serde_json::from_str::<ApiType>(text).unwrap(), expected);
        }
    }

    #[test]
    fn our_own_kebab_spellings_round_trip() {
        for api in [
            ApiType::OpenAiCompletion,
            ApiType::OpenAiResponses,
            ApiType::Anthropic,
            ApiType::Google,
        ] {
            let json = serde_json::to_string(&api).unwrap();
            assert_eq!(serde_json::from_str::<ApiType>(&json).unwrap(), api);
        }
    }

    #[test]
    fn crush_and_catwalk_spellings_parse() {
        // `openai-compat` is Crush's name for a non-OpenAI OpenAI-shaped endpoint.
        // Same wire format, so it maps to the same variant.
        for text in ["openai", "openai-compat", "OPENAI"] {
            assert_eq!(text.parse::<ApiType>().unwrap(), ApiType::OpenAiCompletion);
        }
        for text in ["gemini", "google-vertex", "vertexai"] {
            assert_eq!(text.parse::<ApiType>().unwrap(), ApiType::Google);
        }
    }

    #[test]
    fn an_unknown_type_names_itself() {
        let error = "cohere".parse::<ApiType>().unwrap_err();
        assert!(error.contains("cohere"));
    }

    #[test]
    fn google_is_flagged_as_not_streaming_tool_calls() {
        // A caller has to buffer for it; better stated than discovered.
        assert!(!ApiType::Google.streams_tool_calls());
        assert!(ApiType::Anthropic.streams_tool_calls());
        assert!(ApiType::OpenAiCompletion.streams_tool_calls());
    }
}
