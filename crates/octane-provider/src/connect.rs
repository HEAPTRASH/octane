//! Provider setup.
//!
//! Backs both `octane connect` and `/connect`, so the two cannot drift. The
//! logic here is pure — it reports what a provider needs and writes a file — and
//! the surfaces only supply input and render output.
//!
//! # What this deliberately does not do
//!
//! Subscription sign-in for Claude Pro/Max or ChatGPT (`RESEARCH.md` §P, §Q).
//! Anthropic sent legal demands over exactly that and now blocks it; OpenAI
//! documents the flow only for its own clients, and the practical route is
//! reusing their client ID, which is impersonation rather than integration.
//!
//! What is offered instead: API keys, and `tokenFile` for anything minted out of
//! band. A provider that sanctions third-party OAuth becomes a config file, not
//! a code change.

use crate::api::ApiType;
use crate::config::{Auth, Defaults, ModelEntry, ProviderConfig};

/// A provider octane can walk someone through setting up.
#[derive(Debug, Clone, PartialEq)]
pub struct Recipe {
    /// Key the file is written under, and what `--model` takes.
    pub key: &'static str,
    pub name: &'static str,
    /// What the user must supply.
    pub credential: Credential,
    /// Where to get it.
    pub help_url: &'static str,
    pub note: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// An API key, stored as a `${VAR}` reference so the file stays committable.
    ApiKey { env_var: &'static str },
    /// Nothing to supply — a local endpoint.
    None,
    /// A token minted elsewhere and read from disk.
    TokenFile,
}

impl Credential {
    pub fn env_var(&self) -> Option<&'static str> {
        match self {
            Self::ApiKey { env_var } => Some(env_var),
            _ => None,
        }
    }
}

/// Providers offered by `connect`.
pub fn recipes() -> Vec<Recipe> {
    vec![
        Recipe {
            key: "anthropic",
            name: "Anthropic",
            credential: Credential::ApiKey { env_var: "ANTHROPIC_API_KEY" },
            help_url: "https://console.anthropic.com/settings/keys",
            // Stated up front rather than discovered after a failed attempt.
            note: Some(
                "API key only. Claude Pro/Max subscriptions do not cover third-party tools.",
            ),
        },
        Recipe {
            key: "openai",
            name: "OpenAI",
            credential: Credential::ApiKey { env_var: "OPENAI_API_KEY" },
            help_url: "https://platform.openai.com/api-keys",
            note: Some("API key only. ChatGPT subscription sign-in is for OpenAI's own clients."),
        },
        Recipe {
            key: "gemini",
            name: "Google Gemini",
            credential: Credential::ApiKey { env_var: "GEMINI_API_KEY" },
            help_url: "https://aistudio.google.com/apikey",
            note: None,
        },
        Recipe {
            key: "openrouter",
            name: "OpenRouter",
            credential: Credential::ApiKey { env_var: "OPENROUTER_API_KEY" },
            help_url: "https://openrouter.ai/keys",
            note: Some("One key, many models, including models from the providers above."),
        },
        Recipe {
            key: "nvidia",
            name: "NVIDIA NIM",
            credential: Credential::ApiKey { env_var: "NVIDIA_API_KEY" },
            help_url: "https://build.nvidia.com",
            note: Some("Free tier, OpenAI-compatible, hosts many open-weight models."),
        },
        Recipe {
            key: "groq",
            name: "Groq",
            credential: Credential::ApiKey { env_var: "GROQ_API_KEY" },
            help_url: "https://console.groq.com/keys",
            note: None,
        },
        Recipe {
            key: "deepseek",
            name: "DeepSeek",
            credential: Credential::ApiKey { env_var: "DEEPSEEK_API_KEY" },
            help_url: "https://platform.deepseek.com/api_keys",
            note: None,
        },
        Recipe {
            key: "xai",
            name: "xAI",
            credential: Credential::ApiKey { env_var: "XAI_API_KEY" },
            help_url: "https://console.x.ai",
            note: None,
        },
        Recipe {
            key: "ollama",
            name: "Ollama (local)",
            credential: Credential::None,
            help_url: "https://ollama.com/download",
            note: Some("Runs on localhost:11434. Pull a model first, e.g. `ollama pull qwen3-coder`."),
        },
        Recipe {
            key: "vertex",
            name: "Google Vertex AI",
            credential: Credential::TokenFile,
            help_url: "https://cloud.google.com/vertex-ai/docs/authentication",
            note: Some(
                "Write a token to the file, e.g. `gcloud auth print-access-token > ~/.octane/vertex.token`.",
            ),
        },
    ]
}

pub fn recipe(key: &str) -> Option<Recipe> {
    recipes().into_iter().find(|recipe| recipe.key == key)
}

/// Build the provider config a recipe produces.
///
/// The credential is stored as a `${VAR}` reference, never the literal value, so
/// the resulting file is safe to commit and share — which is the whole reason
/// the reference syntax exists.
pub fn build_config(recipe: &Recipe) -> ProviderConfig {
    let (api, base_url, models, defaults) = match recipe.key {
        "anthropic" => (
            ApiType::Anthropic,
            "https://api.anthropic.com/v1",
            vec![
                ("sonnet", "claude-sonnet-4-5", "Claude Sonnet 4.5", 200_000, 64_000, true),
                ("haiku", "claude-haiku-4-5", "Claude Haiku 4.5", 200_000, 32_000, false),
            ],
            ("sonnet", "haiku"),
        ),
        "openai" => (
            ApiType::OpenAiResponses,
            "https://api.openai.com/v1",
            vec![
                ("gpt", "gpt-5", "GPT-5", 400_000, 128_000, true),
                ("mini", "gpt-5-mini", "GPT-5 mini", 400_000, 128_000, false),
            ],
            ("gpt", "mini"),
        ),
        "gemini" => (
            ApiType::Google,
            "https://generativelanguage.googleapis.com/v1beta",
            vec![
                ("pro", "gemini-3-pro", "Gemini 3 Pro", 1_000_000, 64_000, true),
                ("flash", "gemini-3-flash", "Gemini 3 Flash", 1_000_000, 64_000, false),
            ],
            ("pro", "flash"),
        ),
        "openrouter" => (
            ApiType::OpenAiCompletion,
            "https://openrouter.ai/api/v1",
            vec![
                ("sonnet", "anthropic/claude-sonnet-4.5", "Claude Sonnet 4.5", 200_000, 64_000, true),
                ("haiku", "anthropic/claude-haiku-4.5", "Claude Haiku 4.5", 200_000, 32_000, false),
                ("qwen", "qwen/qwen3-coder", "Qwen3 Coder", 262_144, 32_000, false),
            ],
            ("sonnet", "haiku"),
        ),
        "nvidia" => (
            ApiType::OpenAiCompletion,
            "https://integrate.api.nvidia.com/v1",
            vec![
                ("qwen", "qwen/qwen3.5-397b-a17b", "Qwen3.5 397B", 128_000, 16_384, true),
                ("deepseek", "deepseek-ai/deepseek-v4-pro", "DeepSeek V4 Pro", 128_000, 16_384, true),
                ("kimi", "moonshotai/kimi-k2.6", "Kimi K2.6", 128_000, 16_384, true),
                ("gptoss", "openai/gpt-oss-120b", "GPT-OSS 120B", 128_000, 16_384, true),
                ("nemotron", "nvidia/nemotron-3-super-120b-a12b", "Nemotron 3 Super", 128_000, 16_384, true),
                ("llama", "meta/llama-3.3-70b-instruct", "Llama 3.3 70B", 128_000, 16_384, false),
            ],
            ("qwen", "llama"),
        ),
        "groq" => (
            ApiType::OpenAiCompletion,
            "https://api.groq.com/openai/v1",
            vec![("kimi", "moonshotai/kimi-k2-instruct", "Kimi K2", 131_072, 16_384, false)],
            ("kimi", "kimi"),
        ),
        "deepseek" => (
            ApiType::OpenAiCompletion,
            "https://api.deepseek.com/v1",
            vec![("chat", "deepseek-chat", "DeepSeek Chat", 128_000, 8_192, false)],
            ("chat", "chat"),
        ),
        "xai" => (
            ApiType::OpenAiCompletion,
            "https://api.x.ai/v1",
            vec![("grok", "grok-4", "Grok 4", 256_000, 32_000, true)],
            ("grok", "grok"),
        ),
        "ollama" => (
            ApiType::OpenAiCompletion,
            "http://localhost:11434/v1",
            vec![("qwen", "qwen3-coder:latest", "Qwen3 Coder (local)", 262_144, 32_000, false)],
            ("qwen", "qwen"),
        ),
        "vertex" => (
            ApiType::Google,
            "https://us-central1-aiplatform.googleapis.com/v1/projects/${GOOGLE_PROJECT}/locations/us-central1/publishers/google/models",
            vec![("pro", "gemini-3-pro", "Gemini 3 Pro", 1_000_000, 64_000, true)],
            ("pro", "pro"),
        ),
        _ => (ApiType::OpenAiCompletion, "http://localhost/v1", vec![], ("", "")),
    };

    let auth = match &recipe.credential {
        Credential::None => Auth::None,
        Credential::TokenFile => Auth::TokenFile {
            path: "${HOME}/.octane/vertex.token".into(),
            header: "Authorization".into(),
            prefix: "Bearer ".into(),
        },
        Credential::ApiKey { env_var } => {
            let reference = format!("${{{env_var}}}");
            match api {
                ApiType::Anthropic => Auth::anthropic_key(reference),
                ApiType::Google => Auth::gemini_key(reference),
                _ => Auth::bearer(reference),
            }
        }
    };

    let mut headers = std::collections::BTreeMap::new();
    if api == ApiType::Anthropic {
        headers.insert("anthropic-version".to_string(), "2023-06-01".to_string());
    }

    ProviderConfig {
        name: Some(recipe.name.to_string()),
        api: Some(api),
        base_url: Some(base_url.to_string()),
        auth,
        headers,
        defaults: Defaults {
            primary: (!defaults.0.is_empty()).then(|| defaults.0.to_string()),
            faster: (!defaults.1.is_empty()).then(|| defaults.1.to_string()),
        },
        models: models
            .into_iter()
            .map(|(key, id, name, context, output, reasoning)| {
                (
                    key.to_string(),
                    ModelEntry {
                        id: id.to_string(),
                        name: Some(name.to_string()),
                        context_window: Some(context),
                        max_output: Some(output),
                        reasoning: Some(reasoning),
                        images: Some(true),
                        ..Default::default()
                    },
                )
            })
            .collect(),
        ..Default::default()
    }
}

/// Where a recipe's file goes.
pub fn config_path(root: &std::path::Path, recipe: &Recipe) -> std::path::PathBuf {
    root.join(crate::registry::PROVIDERS_DIR).join(format!("{}.json", recipe.key))
}

/// Write the provider file, creating directories as needed.
pub fn write_config(
    root: &std::path::Path,
    recipe: &Recipe,
    config: &ProviderConfig,
) -> std::io::Result<std::path::PathBuf> {
    let path = config_path(root, recipe);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(config)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    std::fs::write(&path, format!("{json}\n"))?;
    Ok(path)
}

/// Whether the credential a recipe needs is already present.
pub fn is_satisfied(recipe: &Recipe, lookup: impl Fn(&str) -> Option<String>) -> bool {
    match &recipe.credential {
        Credential::None => true,
        Credential::TokenFile => false,
        Credential::ApiKey { env_var } => {
            lookup(env_var).is_some_and(|value| !value.trim().is_empty())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variable a recipe might reference, so resolution tests exercise
    /// structure rather than whatever happens to be in the shell.
    fn env(_name: &str) -> Option<String> {
        Some("test-value".into())
    }

    #[test]
    fn every_recipe_builds_a_resolvable_config() {
        for recipe in recipes() {
            let config = build_config(&recipe);
            assert!(!config.models.is_empty(), "{} has no models", recipe.key);

            // Every declared model must resolve, or `connect` writes a file that
            // does not work.
            for key in config.models.keys() {
                let result = config.resolve_with(recipe.key, key, env);
                assert!(result.is_ok(), "{}/{key}: {:?}", recipe.key, result.err());
            }
        }
    }

    #[test]
    fn every_recipe_binds_both_roles() {
        for recipe in recipes() {
            let config = build_config(&recipe);
            for role in [crate::config::Role::Primary, crate::config::Role::Faster] {
                assert!(
                    config.resolve_role_with(recipe.key, role, env).is_ok(),
                    "{} cannot resolve {role:?}",
                    recipe.key
                );
            }
        }
    }

    #[test]
    fn credentials_are_written_as_references_never_literals() {
        // This is what makes the file safe to commit, which is the whole point.
        for recipe in recipes() {
            let config = build_config(&recipe);
            if let Auth::ApiKey { value, .. } = &config.auth {
                assert!(
                    value.starts_with("${") && value.ends_with('}'),
                    "{} stored a literal: {value}",
                    recipe.key
                );
            }
        }
    }

    #[test]
    fn auth_headers_match_what_each_format_wants() {
        let anthropic = build_config(&recipe("anthropic").unwrap());
        assert!(matches!(anthropic.auth, Auth::ApiKey { ref header, .. } if header == "x-api-key"));

        let gemini = build_config(&recipe("gemini").unwrap());
        assert!(matches!(gemini.auth, Auth::ApiKey { ref header, .. } if header == "x-goog-api-key"));

        let openai = build_config(&recipe("openai").unwrap());
        assert!(matches!(openai.auth, Auth::ApiKey { ref header, .. } if header == "Authorization"));
    }

    #[test]
    fn anthropic_gets_its_required_version_header() {
        let config = build_config(&recipe("anthropic").unwrap());
        assert_eq!(config.headers["anthropic-version"], "2023-06-01");
    }

    #[test]
    fn a_local_provider_needs_no_credential() {
        let ollama = recipe("ollama").unwrap();
        assert_eq!(ollama.credential, Credential::None);
        assert!(is_satisfied(&ollama, |_| None));
        assert_eq!(build_config(&ollama).auth, Auth::None);
    }

    #[test]
    fn subscription_limits_are_stated_up_front() {
        // Better than discovering it after a failed sign-in attempt.
        let anthropic = recipe("anthropic").unwrap();
        assert!(anthropic.note.unwrap().contains("do not cover third-party"));

        let openai = recipe("openai").unwrap();
        assert!(openai.note.unwrap().contains("own clients"));
    }

    #[test]
    fn no_recipe_offers_subscription_sign_in() {
        // Anthropic sent legal demands over exactly this; OpenAI documents it
        // only for first-party clients.
        for recipe in recipes() {
            assert!(
                matches!(
                    recipe.credential,
                    Credential::ApiKey { .. } | Credential::None | Credential::TokenFile
                ),
                "{} offers something other than a key, none, or a token file",
                recipe.key
            );
        }
    }

    #[test]
    fn a_satisfied_credential_is_detected() {
        let anthropic = recipe("anthropic").unwrap();
        assert!(is_satisfied(&anthropic, |_| Some("sk-test".into())));
        assert!(!is_satisfied(&anthropic, |_| None));
        // Whitespace is not a key.
        assert!(!is_satisfied(&anthropic, |_| Some("   ".into())));
    }

    #[test]
    fn writing_creates_the_directory_and_a_reloadable_file() {
        let dir = tempfile::tempdir().unwrap();
        let recipe = recipe("openrouter").unwrap();
        let config = build_config(&recipe);

        let path = write_config(dir.path(), &recipe, &config).unwrap();
        assert!(path.ends_with("providers/openrouter.json"));

        // The round trip is what proves `connect` writes something octane reads.
        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded: ProviderConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(reloaded, config);
    }

    #[test]
    fn a_written_file_is_discovered_by_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let recipe = recipe("groq").unwrap();
        write_config(dir.path(), &recipe, &build_config(&recipe)).unwrap();

        let registry = crate::registry::Registry::load(&[dir.path().to_path_buf()]);
        assert!(registry.get("groq").is_some());
        assert!(registry.errors().is_empty());
    }

    #[test]
    fn openai_defaults_to_the_responses_api() {
        // The newer format, and the one OpenAI's own tooling uses.
        let config = build_config(&recipe("openai").unwrap());
        assert_eq!(config.api, Some(ApiType::OpenAiResponses));
    }

    #[test]
    fn openrouter_speaks_completions_even_for_anthropic_models() {
        // It fronts them behind an OpenAI-shaped API, which is exactly why the
        // wire format belongs to the endpoint rather than the model vendor.
        let config = build_config(&recipe("openrouter").unwrap());
        assert_eq!(config.api, Some(ApiType::OpenAiCompletion));
        assert_eq!(config.models["sonnet"].id, "anthropic/claude-sonnet-4.5");
    }
}
