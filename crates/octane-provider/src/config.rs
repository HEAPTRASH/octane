//! Provider and model configuration.
//!
//! Junie's `${VAR}` references and merge semantics (`RESEARCH.md` §M), but
//! organized around **providers** rather than single models, so one file
//! declares a connection and every model reachable through it. Junie needs a
//! file per model; catwalk gets this right with a `models` array and is the
//! shape followed here.
//!
//! ```json
//! {
//!   "name": "OpenRouter",
//!   "api": "openai-completion",
//!   "baseUrl": "https://openrouter.ai/api/v1",
//!   "auth": { "type": "apiKey", "value": "${OPENROUTER_API_KEY}" },
//!   "defaults": { "primary": "sonnet", "faster": "haiku" },
//!   "models": {
//!     "sonnet": { "id": "anthropic/claude-sonnet-4.5", "contextWindow": 200000 },
//!     "haiku":  { "id": "anthropic/claude-haiku-4.5" }
//!   }
//! }
//! ```
//!
//! # Why `api` and `baseUrl` are per-model too
//!
//! catwalk and Junie both fix the wire format at the provider, which does not
//! survive contact with real gateways. A single endpoint commonly fronts several
//! shapes at once — `/v1/chat/completions`, `/v1/responses`, and
//! `/v1/messages` — and Google's two flavours share a format while differing in
//! URL and auth entirely. So both fields are provider *defaults* that any model
//! may override:
//!
//! ```json
//! {
//!   "baseUrl": "https://gateway.corp/v1",
//!   "api": "openai-completion",
//!   "models": {
//!     "gpt":    { "id": "gpt-5" },
//!     "o-next": { "id": "o-next", "api": "openai-responses" },
//!     "claude": { "id": "claude-sonnet-4.5", "api": "anthropic",
//!                 "baseUrl": "https://gateway.corp/anthropic/v1" }
//!   }
//! }
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::api::ApiType;
use crate::pricing::Pricing;

/// Which model a request is for.
///
/// Summarization, classification, and session titles do not need the model doing
/// the reasoning. Pointing them at a cheaper one is a real cost lever that costs
/// nothing to expose — Junie calls these `primaryModel`/`fasterModel`, catwalk
/// `default_large_model_id`/`default_small_model_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Primary,
    Faster,
}

/// How to authenticate.
///
/// Typed rather than a bare key string, because the enterprise endpoints people
/// actually need are not header-and-token: Vertex wants an OAuth token derived
/// from application default credentials plus a project and location in the URL,
/// and Bedrock wants a SigV4 signature over the request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Auth {
    /// Local endpoints — Ollama, LM Studio, llama.cpp.
    #[default]
    None,

    /// A token in a header.
    ///
    /// `header` and `prefix` are configurable because the three major formats
    /// disagree: OpenAI wants `Authorization: Bearer …`, Anthropic wants
    /// `x-api-key: …` with no prefix, and Gemini wants `x-goog-api-key: …`.
    #[serde(rename_all = "camelCase")]
    ApiKey {
        /// Supports `${VAR}` references.
        value: String,
        #[serde(default = "default_auth_header")]
        header: String,
        #[serde(default = "default_auth_prefix")]
        prefix: String,
    },

    /// Google Vertex AI. Same wire format as Gemini, different URL and auth.
    #[serde(rename_all = "camelCase")]
    GoogleVertex {
        project: String,
        #[serde(default = "default_vertex_location")]
        location: String,
        /// Service-account JSON path. Falls back to application default
        /// credentials when unset, which is what `gcloud auth` leaves behind.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credentials: Option<String>,
    },

    /// AWS Bedrock, signed with SigV4.
    #[serde(rename_all = "camelCase")]
    AwsSigV4 {
        region: String,
        /// Named profile from `~/.aws/credentials`. Falls back to the standard
        /// provider chain when unset.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },

    /// A bearer token read from a file, refreshed out of band.
    ///
    /// Covers OAuth flows without this crate having to own one: whatever mints
    /// the token writes it here.
    #[serde(rename_all = "camelCase")]
    TokenFile {
        path: String,
        #[serde(default = "default_auth_header")]
        header: String,
        #[serde(default = "default_auth_prefix")]
        prefix: String,
    },
}

fn default_auth_header() -> String {
    "Authorization".to_string()
}

fn default_auth_prefix() -> String {
    "Bearer ".to_string()
}

fn default_vertex_location() -> String {
    "us-central1".to_string()
}

impl Auth {
    /// Auth shaped for an Anthropic endpoint: `x-api-key`, no prefix.
    pub fn anthropic_key(value: impl Into<String>) -> Self {
        Self::ApiKey {
            value: value.into(),
            header: "x-api-key".into(),
            prefix: String::new(),
        }
    }

    /// Auth shaped for the Gemini API: `x-goog-api-key`, no prefix.
    pub fn gemini_key(value: impl Into<String>) -> Self {
        Self::ApiKey {
            value: value.into(),
            header: "x-goog-api-key".into(),
            prefix: String::new(),
        }
    }

    /// Auth shaped for OpenAI and the endpoints that copy it.
    pub fn bearer(value: impl Into<String>) -> Self {
        Self::ApiKey {
            value: value.into(),
            header: default_auth_header(),
            prefix: default_auth_prefix(),
        }
    }

    /// Whether this can be satisfied without reaching out to a credential
    /// provider — used to fail fast at load rather than at first request.
    pub fn is_static(&self) -> bool {
        matches!(self, Self::None | Self::ApiKey { .. })
    }

    /// Expand `${VAR}` references in whatever this variant carries.
    pub fn resolve_env(self, provider: &str) -> Result<Self, ConfigError> {
        Ok(match self {
            Self::ApiKey { value, header, prefix } => Self::ApiKey {
                value: resolve_env(&value, provider)?,
                header: resolve_env(&header, provider)?,
                prefix,
            },
            Self::GoogleVertex { project, location, credentials } => Self::GoogleVertex {
                project: resolve_env(&project, provider)?,
                location: resolve_env(&location, provider)?,
                credentials: credentials
                    .map(|path| resolve_env(&path, provider))
                    .transpose()?,
            },
            Self::AwsSigV4 { region, profile } => Self::AwsSigV4 {
                region: resolve_env(&region, provider)?,
                profile,
            },
            Self::TokenFile { path, header, prefix } => Self::TokenFile {
                path: resolve_env(&path, provider)?,
                header,
                prefix,
            },
            Self::None => Self::None,
        })
    }
}

/// One model, as written under a provider's `models`.
///
/// Everything but `id` is optional and falls back to the provider.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelEntry {
    /// Identifier the endpoint expects, e.g. `anthropic/claude-sonnet-4.5`.
    pub id: String,

    /// Display name. Defaults to the key this entry is filed under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Overrides the provider's wire format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ApiType>,

    /// Overrides the provider's endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// Overrides the provider's auth entirely — for a gateway whose Anthropic
    /// route wants a different key from its OpenAI route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u64>,

    /// Left unset by default so the provider's own default applies. A hardcoded
    /// value silently overrides whatever it tuned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<Pricing>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<bool>,

    /// Whether the endpoint needs explicit cache breakpoints (Anthropic-style)
    /// rather than caching prefixes on its own (OpenAI-style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explicit_cache_control: Option<bool>,
}

/// Which model each role uses, by key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Defaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faster: Option<String>,
}

/// A provider file: one connection, many models.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderConfig {
    /// Display name. Defaults to the filename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Default wire format for this provider's models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ApiType>,

    /// Default endpoint. May be a host-and-version prefix; the format's path is
    /// appended when it looks like one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    #[serde(default)]
    pub auth: Auth,

    /// Sent with every request. Merged with a model's own, model wins.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,

    /// Merged into every request body. Merged recursively with a model's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    #[serde(default)]
    pub defaults: Defaults,

    /// Keyed by short name, which is what the user types after `provider/`.
    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,

    /// Skip this provider without deleting the file.
    #[serde(default)]
    pub disabled: bool,
}

/// A model with provider defaults applied and every field decided.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModel {
    /// Provider key, from the filename.
    pub provider: String,
    /// Model key within the provider.
    pub key: String,
    /// `provider/key`, which is what `--model` takes.
    pub reference: String,
    pub display_name: String,

    pub model_id: String,
    pub api: ApiType,
    pub base_url: String,
    pub auth: Auth,
    pub headers: BTreeMap<String, String>,
    pub body: Option<serde_json::Value>,

    pub context_window: u64,
    pub max_output: u64,
    pub temperature: Option<f32>,
    pub pricing: Pricing,
    pub reasoning: bool,
    pub images: bool,
    pub explicit_cache_control: bool,
}

/// Context window assumed when nothing says otherwise.
///
/// Conservative on purpose: guessing high means compaction never fires and the
/// first the user hears of it is a provider error mid-task.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;
pub const DEFAULT_MAX_OUTPUT: u64 = 8_192;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: {source}")]
    Io { path: String, #[source] source: std::io::Error },

    #[error("{path}: {source}")]
    Parse { path: String, #[source] source: serde_json::Error },

    #[error("provider {provider:?} references ${{{variable}}}, which is not set")]
    MissingEnv { provider: String, variable: String },

    #[error("provider {provider:?} declares no models")]
    NoModels { provider: String },

    #[error("provider {provider:?} has no {field}, and model {model:?} does not supply one")]
    Incomplete { provider: String, model: String, field: &'static str },

    #[error("provider {provider:?} names {role} model {key:?}, which it does not declare")]
    UnknownRoleModel { provider: String, role: &'static str, key: String },

    #[error("no model matches {0:?}")]
    UnknownModel(String),
}

impl ProviderConfig {
    /// Resolve one model.
    pub fn resolve(&self, provider: &str, key: &str) -> Result<ResolvedModel, ConfigError> {
        let entry = self.models.get(key).ok_or_else(|| {
            ConfigError::UnknownModel(format!("{provider}/{key}"))
        })?;

        let api = entry.api.or(self.api).ok_or(ConfigError::Incomplete {
            provider: provider.into(),
            model: key.into(),
            field: "api",
        })?;

        let base_url = entry
            .base_url
            .clone()
            .or_else(|| self.base_url.clone())
            .ok_or(ConfigError::Incomplete {
                provider: provider.into(),
                model: key.into(),
                field: "baseUrl",
            })?;

        // Headers merge, model wins conflicts.
        let mut headers = self.headers.clone();
        headers.extend(entry.headers.clone());
        for value in headers.values_mut() {
            *value = resolve_env(value, provider)?;
        }

        // Body merges recursively, so a model can override one nested key
        // without discarding the rest of the subtree.
        let body = match (&self.body, &entry.body) {
            (Some(base), Some(model_body)) => Some(merge_json(base.clone(), model_body)),
            (Some(base), None) => Some(base.clone()),
            (None, model_body) => model_body.clone(),
        };

        let auth = entry
            .auth
            .clone()
            .unwrap_or_else(|| self.auth.clone())
            .resolve_env(provider)?;

        Ok(ResolvedModel {
            provider: provider.to_string(),
            key: key.to_string(),
            reference: format!("{provider}/{key}"),
            display_name: entry.name.clone().unwrap_or_else(|| key.to_string()),
            model_id: entry.id.clone(),
            api,
            base_url: normalize_url(&base_url, api),
            auth,
            headers,
            body,
            context_window: entry.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW),
            max_output: entry.max_output.unwrap_or(DEFAULT_MAX_OUTPUT),
            temperature: entry.temperature.or(self.temperature),
            pricing: entry.pricing.unwrap_or_default(),
            reasoning: entry.reasoning.unwrap_or(false),
            images: entry.images.unwrap_or(false),
            // Anthropic needs explicit cache breakpoints and OpenAI does not, so
            // the format is the right default when nothing says otherwise.
            explicit_cache_control: entry
                .explicit_cache_control
                .unwrap_or(matches!(api, ApiType::Anthropic)),
        })
    }

    /// Resolve the model bound to a role.
    ///
    /// Falls back to the sole model when only one is declared, and otherwise to
    /// the primary binding — so a single-model provider needs no `defaults` block
    /// and a provider that names only a primary still answers for `faster`.
    pub fn resolve_role(&self, provider: &str, role: Role) -> Result<ResolvedModel, ConfigError> {
        if self.models.is_empty() {
            return Err(ConfigError::NoModels { provider: provider.into() });
        }

        let (bound, label) = match role {
            Role::Primary => (self.defaults.primary.as_ref(), "primary"),
            Role::Faster => (
                self.defaults.faster.as_ref().or(self.defaults.primary.as_ref()),
                "faster",
            ),
        };

        let key = match bound {
            Some(key) => {
                if !self.models.contains_key(key) {
                    return Err(ConfigError::UnknownRoleModel {
                        provider: provider.into(),
                        role: label,
                        key: key.clone(),
                    });
                }
                key.clone()
            }
            // BTreeMap, so "first" is stable rather than whatever the hash
            // happened to produce this run.
            None => self.models.keys().next().expect("non-empty").clone(),
        };

        self.resolve(provider, &key)
    }

    /// Every model this provider declares, resolved.
    pub fn resolve_all(&self, provider: &str) -> Vec<Result<ResolvedModel, ConfigError>> {
        self.models.keys().map(|key| self.resolve(provider, key)).collect()
    }
}

/// Append the format's path when `base_url` looks like a host-and-version prefix.
///
/// People copy `https://api.deepseek.com/v1` out of documentation and expect it
/// to work. Requiring the full endpoint is a papercut; guessing wrongly when a
/// full endpoint *was* given is worse, so the heuristic only fires when the URL
/// does not already end in a known path.
fn normalize_url(base_url: &str, api: ApiType) -> String {
    let trimmed = base_url.trim_end_matches('/');
    let path = api.default_path();

    if path.is_empty() || trimmed.ends_with(path) {
        return trimmed.to_string();
    }
    // A URL already naming some other endpoint is left alone: it is more likely
    // a deliberate route than a mistake.
    if ["/chat/completions", "/responses", "/messages", ":streamGenerateContent"]
        .iter()
        .any(|known| trimmed.ends_with(known))
    {
        return trimmed.to_string();
    }
    format!("{trimmed}{path}")
}

/// Expand `${VAR}` references against the process environment.
///
/// A missing variable is an error naming it, not a silently empty value. The
/// alternative surfaces as a 401 that looks like a bad key, and people lose an
/// afternoon to it. This is what makes a provider file safe to commit.
pub fn resolve_env(value: &str, provider: &str) -> Result<String, ConfigError> {
    resolve_env_with(value, provider, |name| std::env::var(name).ok())
}

/// Expand `${VAR}` references against an arbitrary lookup.
///
/// The lookup is injected rather than read directly so this is testable without
/// mutating the process environment — which is both `unsafe` in edition 2024 and
/// shared mutable state between concurrently running tests.
pub fn resolve_env_with(
    value: &str,
    provider: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];

        let Some(end) = after.find('}') else {
            // Unterminated: literal text. `${` is legal in a key that has it.
            out.push_str(&rest[start..]);
            return Ok(out);
        };

        let name = &after[..end];
        if !is_env_name(name) {
            out.push_str(&rest[start..start + 2 + end + 1]);
            rest = &after[end + 1..];
            continue;
        }

        let resolved = lookup(name).ok_or_else(|| ConfigError::MissingEnv {
            provider: provider.to_string(),
            variable: name.to_string(),
        })?;
        out.push_str(&resolved);
        rest = &after[end + 1..];
    }

    out.push_str(rest);
    Ok(out)
}

/// `${NAME}` where NAME starts with a letter or underscore and continues with
/// letters, digits, or underscores. Anything else is left alone, so shell-ish
/// text in a header value survives.
fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Recursive JSON merge. `overlay` wins, except where both sides are objects.
pub fn merge_json(mut base: serde_json::Value, overlay: &serde_json::Value) -> serde_json::Value {
    match (&mut base, overlay) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(overlay_map)) => {
            for (key, overlay_value) in overlay_map {
                let merged = match base_map.remove(key) {
                    Some(existing) => merge_json(existing, overlay_value),
                    None => overlay_value.clone(),
                };
                base_map.insert(key.clone(), merged);
            }
            base
        }
        _ => overlay.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: &str) -> ProviderConfig {
        serde_json::from_str(json).expect("valid provider config")
    }

    const GATEWAY: &str = r#"{
        "name": "Corp Gateway",
        "api": "openai-completion",
        "baseUrl": "https://gateway.corp/v1",
        "auth": { "type": "apiKey", "value": "sk-gateway" },
        "headers": { "X-Team": "platform" },
        "defaults": { "primary": "claude", "faster": "mini" },
        "models": {
            "gpt":    { "id": "gpt-5" },
            "mini":   { "id": "gpt-5-mini" },
            "o-next": { "id": "o-next", "api": "openai-responses" },
            "claude": {
                "id": "claude-sonnet-4-5",
                "api": "anthropic",
                "baseUrl": "https://gateway.corp/anthropic/v1",
                "contextWindow": 200000
            }
        }
    }"#;

    /// A stand-in environment, so tests never mutate the real one.
    fn fake_env(name: &str) -> Option<String> {
        match name {
            "OCTANE_TEST_GW_KEY" => Some("sk-gateway".into()),
            "OCTANE_TEST_KEY2" => Some("sk-secret".into()),
            "OCTANE_TEST_HDR" => Some("hv".into()),
            _ => None,
        }
    }

    fn resolve(value: &str, provider: &str) -> Result<String, ConfigError> {
        resolve_env_with(value, provider, fake_env)
    }

    fn gateway() -> ProviderConfig {
        config(GATEWAY)
    }

    #[test]
    fn one_file_declares_many_models() {
        // The whole point of the provider-centric shape: no file per model.
        let provider = gateway();
        assert_eq!(provider.models.len(), 4);

        let resolved: Vec<String> = provider
            .resolve_all("corp")
            .into_iter()
            .map(|model| model.unwrap().reference)
            .collect();
        assert_eq!(resolved, vec!["corp/claude", "corp/gpt", "corp/mini", "corp/o-next"]);
    }

    #[test]
    fn models_inherit_the_provider_connection() {
        let model = gateway().resolve("corp", "gpt").unwrap();
        assert_eq!(model.api, ApiType::OpenAiCompletion);
        assert_eq!(model.base_url, "https://gateway.corp/v1/chat/completions");
        assert_eq!(model.headers["X-Team"], "platform");
    }

    #[test]
    fn a_model_can_override_the_wire_format() {
        // One gateway fronting several shapes is the case catwalk and Junie
        // cannot express.
        let provider = gateway();
        assert_eq!(provider.resolve("corp", "gpt").unwrap().api, ApiType::OpenAiCompletion);
        assert_eq!(
            provider.resolve("corp", "o-next").unwrap().api,
            ApiType::OpenAiResponses
        );
        assert_eq!(provider.resolve("corp", "claude").unwrap().api, ApiType::Anthropic);
    }

    #[test]
    fn a_model_can_override_the_endpoint() {
        let model = gateway().resolve("corp", "claude").unwrap();
        assert_eq!(model.base_url, "https://gateway.corp/anthropic/v1/messages");
        // ...while still inheriting the provider's headers and auth.
        assert_eq!(model.headers["X-Team"], "platform");
        assert!(matches!(model.auth, Auth::ApiKey { .. }));
    }

    #[test]
    fn the_format_path_is_appended_to_a_version_prefix() {
        // People copy `https://api.deepseek.com/v1` out of the docs.
        for (base, api, expected) in [
            ("https://x/v1", ApiType::OpenAiCompletion, "https://x/v1/chat/completions"),
            ("https://x/v1/", ApiType::OpenAiCompletion, "https://x/v1/chat/completions"),
            ("https://x/v1", ApiType::Anthropic, "https://x/v1/messages"),
            ("https://x/v1", ApiType::OpenAiResponses, "https://x/v1/responses"),
        ] {
            assert_eq!(normalize_url(base, api), expected);
        }
    }

    #[test]
    fn a_full_endpoint_url_is_left_alone() {
        // Guessing when a complete URL was given is worse than not guessing.
        assert_eq!(
            normalize_url("https://x/v1/chat/completions", ApiType::OpenAiCompletion),
            "https://x/v1/chat/completions"
        );
        // Even when it names a *different* endpoint — more likely a deliberate
        // route than a mistake.
        assert_eq!(
            normalize_url("https://x/v1/messages", ApiType::OpenAiCompletion),
            "https://x/v1/messages"
        );
    }

    #[test]
    fn roles_resolve_to_the_bound_models() {
        let provider = gateway();
        assert_eq!(provider.resolve_role("corp", Role::Primary).unwrap().key, "claude");
        assert_eq!(provider.resolve_role("corp", Role::Faster).unwrap().key, "mini");
    }

    #[test]
    fn faster_falls_back_to_primary_when_unbound() {
        let provider = config(
            r#"{
                "api": "anthropic", "baseUrl": "https://x/v1",
                "defaults": { "primary": "big" },
                "models": { "big": { "id": "b" }, "small": { "id": "s" } }
            }"#,
        );
        assert_eq!(provider.resolve_role("p", Role::Faster).unwrap().key, "big");
    }

    #[test]
    fn a_single_model_provider_needs_no_defaults_block() {
        let provider = config(
            r#"{ "api": "anthropic", "baseUrl": "https://x/v1",
                 "models": { "only": { "id": "m" } } }"#,
        );
        assert_eq!(provider.resolve_role("p", Role::Primary).unwrap().key, "only");
        assert_eq!(provider.resolve_role("p", Role::Faster).unwrap().key, "only");
    }

    #[test]
    fn a_role_naming_a_missing_model_says_so() {
        let provider = config(
            r#"{ "api": "anthropic", "baseUrl": "https://x/v1",
                 "defaults": { "primary": "typo" },
                 "models": { "real": { "id": "m" } } }"#,
        );
        let error = provider.resolve_role("p", Role::Primary).unwrap_err();
        assert!(matches!(error, ConfigError::UnknownRoleModel { .. }));
        assert!(error.to_string().contains("typo"));
    }

    #[test]
    fn a_provider_with_no_models_is_an_error_not_an_empty_list() {
        let provider = config(r#"{ "api": "anthropic", "baseUrl": "https://x/v1" }"#);
        assert!(matches!(
            provider.resolve_role("p", Role::Primary).unwrap_err(),
            ConfigError::NoModels { .. }
        ));
    }

    #[test]
    fn headers_merge_with_the_model_winning() {
        let provider = config(
            r#"{
                "api": "anthropic", "baseUrl": "https://x/v1",
                "headers": { "X-Shared": "provider", "X-Only-Provider": "p" },
                "models": { "m": { "id": "m",
                    "headers": { "X-Shared": "model", "X-Only-Model": "m" } } }
            }"#,
        );
        let model = provider.resolve("p", "m").unwrap();
        assert_eq!(model.headers["X-Shared"], "model");
        assert_eq!(model.headers["X-Only-Provider"], "p");
        assert_eq!(model.headers["X-Only-Model"], "m");
    }

    #[test]
    fn body_merges_recursively_rather_than_replacing_a_subtree() {
        let provider = config(
            r#"{
                "api": "openai-completion", "baseUrl": "https://x/v1",
                "body": { "routing": { "region": "eu", "tier": "standard" }, "tag": "a" },
                "models": { "m": { "id": "m", "body": { "routing": { "tier": "cheap" } } } }
            }"#,
        );
        let body = provider.resolve("p", "m").unwrap().body.unwrap();
        assert_eq!(body["routing"]["tier"], "cheap");
        assert_eq!(body["routing"]["region"], "eu", "the subtree must survive");
        assert_eq!(body["tag"], "a");
    }

    #[test]
    fn auth_defaults_to_a_bearer_header_and_is_shapeable() {
        assert!(matches!(
            Auth::bearer("k"),
            Auth::ApiKey { ref header, ref prefix, .. } if header == "Authorization" && prefix == "Bearer "
        ));
        // Anthropic and Gemini disagree with OpenAI and with each other.
        assert!(matches!(
            Auth::anthropic_key("k"),
            Auth::ApiKey { ref header, ref prefix, .. } if header == "x-api-key" && prefix.is_empty()
        ));
        assert!(matches!(
            Auth::gemini_key("k"),
            Auth::ApiKey { ref header, .. } if header == "x-goog-api-key"
        ));
    }

    #[test]
    fn a_model_can_override_auth_entirely() {
        // A gateway whose Anthropic route wants a different key.
        let provider = config(
            r#"{
                "api": "openai-completion", "baseUrl": "https://x/v1",
                "auth": { "type": "apiKey", "value": "shared" },
                "models": { "m": { "id": "m",
                    "auth": { "type": "apiKey", "value": "special", "header": "x-api-key", "prefix": "" } } }
            }"#,
        );
        let model = provider.resolve("p", "m").unwrap();
        match model.auth {
            Auth::ApiKey { value, header, .. } => {
                assert_eq!(value, "special");
                assert_eq!(header, "x-api-key");
            }
            other => panic!("expected an api key, got {other:?}"),
        }
    }

    #[test]
    fn vertex_and_bedrock_auth_are_expressible() {
        let vertex: Auth = serde_json::from_str(
            r#"{ "type": "googleVertex", "project": "my-proj", "location": "europe-west4" }"#,
        )
        .unwrap();
        assert!(matches!(vertex, Auth::GoogleVertex { ref location, .. } if location == "europe-west4"));
        // Neither can be satisfied from the file alone.
        assert!(!vertex.is_static());

        let bedrock: Auth =
            serde_json::from_str(r#"{ "type": "awsSigV4", "region": "us-east-1" }"#).unwrap();
        assert!(!bedrock.is_static());
    }

    #[test]
    fn vertex_defaults_to_a_location_rather_than_failing() {
        let vertex: Auth =
            serde_json::from_str(r#"{ "type": "googleVertex", "project": "p" }"#).unwrap();
        assert!(matches!(vertex, Auth::GoogleVertex { ref location, .. } if location == "us-central1"));
    }

    #[test]
    fn local_endpoints_need_no_auth() {
        let provider = config(
            r#"{ "api": "openai-completion", "baseUrl": "http://localhost:11434/v1",
                 "models": { "qwen": { "id": "qwen3-coder:latest" } } }"#,
        );
        let model = provider.resolve("ollama", "qwen").unwrap();
        assert_eq!(model.auth, Auth::None);
        assert!(model.auth.is_static());
    }

    #[test]
    fn env_references_are_expanded() {
        assert_eq!(resolve("${OCTANE_TEST_KEY2}", "p").unwrap(), "sk-secret");
        assert_eq!(resolve("Bearer ${OCTANE_TEST_KEY2}!", "p").unwrap(), "Bearer sk-secret!");
        // Two references in one value.
        assert_eq!(resolve("${OCTANE_TEST_HDR}-${OCTANE_TEST_HDR}", "p").unwrap(), "hv-hv");
    }

    #[test]
    fn a_missing_variable_is_an_error_naming_it() {
        // Otherwise it is an empty header and a 401 that looks like a bad key.
        let error = resolve("${OCTANE_DEFINITELY_UNSET_XYZ}", "mine").unwrap_err();
        assert!(matches!(error, ConfigError::MissingEnv { .. }));
        assert!(error.to_string().contains("OCTANE_DEFINITELY_UNSET_XYZ"));
        assert!(error.to_string().contains("mine"));
    }

    #[test]
    fn literal_values_and_shell_ish_text_pass_through() {
        assert_eq!(resolve("sk-ant-literal", "p").unwrap(), "sk-ant-literal");
        assert_eq!(resolve("${not-a-name}", "p").unwrap(), "${not-a-name}");
        assert_eq!(resolve("${}", "p").unwrap(), "${}");
        assert_eq!(resolve("a ${unterminated", "p").unwrap(), "a ${unterminated");
    }

    #[test]
    fn cache_control_defaults_to_what_the_format_needs() {
        // Anthropic wants explicit breakpoints; OpenAI caches prefixes itself.
        let provider = gateway();
        assert!(provider.resolve("corp", "claude").unwrap().explicit_cache_control);
        assert!(!provider.resolve("corp", "gpt").unwrap().explicit_cache_control);
    }

    #[test]
    fn temperature_stays_unset_unless_configured() {
        assert_eq!(gateway().resolve("corp", "gpt").unwrap().temperature, None);
    }

    #[test]
    fn a_model_inherits_the_provider_temperature_and_can_override_it() {
        let provider = config(
            r#"{
                "api": "openai-completion", "baseUrl": "https://x/v1",
                "temperature": 0.7,
                "models": { "a": { "id": "a" }, "b": { "id": "b", "temperature": 0.0 } }
            }"#,
        );
        assert_eq!(provider.resolve("p", "a").unwrap().temperature, Some(0.7));
        assert_eq!(provider.resolve("p", "b").unwrap().temperature, Some(0.0));
    }

    #[test]
    fn unknown_fields_are_rejected_rather_than_ignored() {
        // A typo'd key that silently does nothing is worse than a load error.
        assert!(
            serde_json::from_str::<ProviderConfig>(
                r#"{ "api": "anthropic", "baseUrll": "https://x" }"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ModelEntry>(r#"{ "id": "m", "contextWindwo": 1 }"#).is_err()
        );
    }

    #[test]
    fn an_unknown_model_reference_names_it() {
        let error = gateway().resolve("corp", "nope").unwrap_err();
        assert!(error.to_string().contains("corp/nope"));
    }

    #[test]
    fn a_config_round_trips_through_json() {
        let original = gateway();
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(serde_json::from_str::<ProviderConfig>(&json).unwrap(), original);
    }

    #[test]
    fn json_merge_is_recursive_for_objects_and_replacing_otherwise() {
        let base = serde_json::json!({ "a": { "x": 1, "y": 2 }, "b": [1, 2] });
        let overlay = serde_json::json!({ "a": { "y": 99 }, "b": [9] });
        let merged = merge_json(base, &overlay);

        assert_eq!(merged["a"]["x"], 1);
        assert_eq!(merged["a"]["y"], 99);
        // Arrays replace; merging element-wise is never what anyone means.
        assert_eq!(merged["b"], serde_json::json!([9]));
    }
}
