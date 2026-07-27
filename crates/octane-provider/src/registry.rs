//! Finding and resolving providers.
//!
//! Discovery mirrors Junie's, adapted to the provider-centric shape: JSON files
//! in `~/.octane/providers/*.json` (user) and `.octane/providers/*.json`
//! (project), with the filename as the provider key. Project files override user
//! files of the same name, so a repository can pin a gateway for everyone
//! working in it.
//!
//! A handful of well-known providers are built in, so `OPENAI_API_KEY` in the
//! environment is enough to get started without writing any JSON. They are
//! ordinary [`ProviderConfig`] values and a file of the same name replaces them
//! wholesale — nothing is special-cased in code.

use std::collections::BTreeMap;

use crate::config::{Auth, ConfigError, ProviderConfig, ResolvedModel, Role};

/// Directory searched under each config root.
pub const PROVIDERS_DIR: &str = "providers";

#[derive(Debug, Default)]
pub struct Registry {
    providers: BTreeMap<String, ProviderConfig>,
    /// Problems found while loading, surfaced rather than swallowed.
    errors: Vec<ConfigError>,
}

impl Registry {
    /// Built-in providers only.
    pub fn builtin() -> Self {
        Self { providers: builtin_providers(), errors: Vec::new() }
    }

    /// Built-ins, then user files, then project files.
    ///
    /// Later sources replace earlier ones entirely rather than merging, because
    /// a half-overridden connection is harder to reason about than a replaced
    /// one — and merging would make it impossible to *remove* a model.
    pub fn load(roots: &[std::path::PathBuf]) -> Self {
        let mut registry = Self::builtin();
        for root in roots {
            registry.load_dir(&root.join(PROVIDERS_DIR));
        }
        registry
    }

    fn load_dir(&mut self, dir: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            // A missing directory is the normal case, not a problem.
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(key) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };

            match std::fs::read_to_string(&path) {
                Ok(text) => match serde_json::from_str::<ProviderConfig>(&text) {
                    Ok(config) => {
                        self.providers.insert(key.to_string(), config);
                    }
                    // Reported, not fatal: one malformed file must not stop a
                    // session, and the user needs to know which one it was.
                    Err(source) => self.errors.push(ConfigError::Parse {
                        path: path.display().to_string(),
                        source,
                    }),
                },
                Err(source) => self.errors.push(ConfigError::Io {
                    path: path.display().to_string(),
                    source,
                }),
            }
        }
    }

    pub fn errors(&self) -> &[ConfigError] {
        &self.errors
    }

    pub fn get(&self, provider: &str) -> Option<&ProviderConfig> {
        self.providers.get(provider)
    }

    /// Providers that are enabled and have their credentials available.
    ///
    /// A built-in whose API key is unset is filtered out rather than listed and
    /// failing on use — `/models` should show what will work.
    pub fn available(&self) -> Vec<(&str, &ProviderConfig)> {
        self.providers
            .iter()
            .filter(|(key, config)| !config.disabled && self.is_usable(key, config))
            .map(|(key, config)| (key.as_str(), config))
            .collect()
    }

    /// Providers that are configured but cannot be used, with the reason.
    ///
    /// Filtering silently is worse than not listing: someone who wrote a
    /// provider file and does not see it needs to know it was their unset
    /// variable, not a bug.
    pub fn unavailable(&self) -> Vec<(&str, String)> {
        self.providers
            .iter()
            .filter(|(_, config)| !config.disabled)
            .filter_map(|(key, config)| {
                self.unusable_reason(key, config).map(|reason| (key.as_str(), reason))
            })
            .collect()
    }

    fn unusable_reason(&self, key: &str, config: &ProviderConfig) -> Option<String> {
        match &config.auth {
            Auth::ApiKey { value, .. } => match crate::config::resolve_env(value, key) {
                Err(error) => Some(error.to_string()),
                Ok(resolved) if resolved.trim().is_empty() => {
                    Some(format!("provider {key:?} has an empty API key"))
                }
                Ok(_) => None,
            },
            Auth::TokenFile { path, .. } if !std::path::Path::new(path).exists() => {
                Some(format!("provider {key:?} token file {path:?} does not exist"))
            }
            _ => None,
        }
    }

    fn is_usable(&self, key: &str, config: &ProviderConfig) -> bool {
        match &config.auth {
            Auth::None => true,
            // Non-static auth is resolved at request time, so it cannot be
            // checked here without doing real work.
            Auth::GoogleVertex { .. } | Auth::AwsSigV4 { .. } => true,
            Auth::TokenFile { .. } | Auth::ApiKey { .. } => {
                self.unusable_reason(key, config).is_none()
            }
        }
    }

    /// Every resolvable model, as `provider/key` references.
    pub fn models(&self) -> Vec<ResolvedModel> {
        self.available()
            .into_iter()
            .flat_map(|(key, config)| {
                config.resolve_all(key).into_iter().filter_map(Result::ok)
            })
            .collect()
    }

    /// Resolve a `provider/model` reference.
    ///
    /// A bare name is accepted when it is unambiguous — either a provider (whose
    /// primary model is used) or a model key unique across providers. Ambiguity
    /// is an error listing the candidates rather than a silent pick.
    pub fn resolve(&self, reference: &str) -> Result<ResolvedModel, ConfigError> {
        if let Some((provider, model)) = reference.split_once('/') {
            // Model ids often contain slashes themselves (`anthropic/claude-…`),
            // so a provider match takes precedence over splitting further.
            if let Some(config) = self.providers.get(provider) {
                return config.resolve(provider, model);
            }
        }

        if let Some(config) = self.providers.get(reference) {
            return config.resolve_role(reference, Role::Primary);
        }

        let matches: Vec<ResolvedModel> = self
            .available()
            .into_iter()
            .filter_map(|(key, config)| config.resolve(key, reference).ok())
            .collect();

        match matches.len() {
            1 => Ok(matches.into_iter().next().expect("checked")),
            0 => Err(ConfigError::UnknownModel(reference.to_string())),
            _ => Err(ConfigError::UnknownModel(format!(
                "{reference} is ambiguous: {}",
                matches
                    .iter()
                    .map(|m| m.reference.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// Resolve a role against a provider, or the first usable provider.
    pub fn resolve_role(
        &self,
        provider: Option<&str>,
        role: Role,
    ) -> Result<ResolvedModel, ConfigError> {
        match provider {
            Some(key) => self
                .providers
                .get(key)
                .ok_or_else(|| ConfigError::UnknownModel(key.to_string()))?
                .resolve_role(key, role),
            None => {
                let (key, config) = self
                    .available()
                    .into_iter()
                    .find(|(_, config)| !config.models.is_empty())
                    .ok_or_else(|| ConfigError::UnknownModel("no configured provider".into()))?;
                config.resolve_role(key, role)
            }
        }
    }
}

/// Providers that work from an environment variable alone.
///
/// Deliberately small. This is a convenience for the common case, not a registry
/// to maintain — a catalogue of every provider and model goes stale the moment it
/// ships, which is what models.dev exists to solve.
///
/// Built from the same recipes `octane connect` writes to a file, because these
/// were two hand-written copies of one table and had already drifted: the
/// built-in `openai` spoke the Completions API while `octane connect openai`
/// wrote Responses, so the same key behaved differently depending on whether a
/// file existed. One table cannot disagree with itself.
///
/// Responses is the side that won, deliberately: it is what `connect` already
/// wrote, and it is OpenAI's current API. A zero-config `OPENAI_API_KEY` session
/// therefore changed endpoint here — the alternative was leaving two answers to
/// one question. Ollama gained a `defaults` block in the same move; it was the
/// one built-in that named no primary model, so `--model ollama` resolved nothing.
fn builtin_providers() -> BTreeMap<String, ProviderConfig> {
    ["anthropic", "openai", "gemini", "ollama"]
        .into_iter()
        .filter_map(crate::connect::recipe)
        .map(|recipe| (recipe.key.to_string(), crate::connect::build_config(&recipe)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_with(entries: &[(&str, &str)]) -> Registry {
        let mut registry = Registry { providers: BTreeMap::new(), errors: Vec::new() };
        for (key, json) in entries {
            registry
                .providers
                .insert((*key).to_string(), serde_json::from_str(json).expect("valid"));
        }
        registry
    }

    const LOCAL: &str = r#"{
        "api": "openai-completion",
        "baseUrl": "http://localhost:11434/v1",
        "defaults": { "primary": "big" },
        "models": { "big": { "id": "b" }, "small": { "id": "s" } }
    }"#;

    #[test]
    fn builtins_cover_the_common_providers() {
        let registry = Registry::builtin();
        for expected in ["anthropic", "openai", "gemini", "ollama"] {
            assert!(registry.get(expected).is_some(), "missing {expected}");
        }
    }

    /// A built-in provider and the file `octane connect` writes for it are the
    /// same provider, so they must be the same configuration. Held apart, they
    /// drifted: the built-in `openai` spoke Completions while connect wrote
    /// Responses, and the API a key reached depended on whether a file existed.
    #[test]
    fn a_builtin_provider_matches_what_connect_would_write_for_it() {
        let registry = Registry::builtin();
        for (key, builtin) in &registry.providers {
            let recipe = crate::connect::recipe(key).expect("builtin has a connect recipe");
            assert_eq!(&crate::connect::build_config(&recipe), builtin, "{key} drifted");
        }
    }

    /// Every built-in names a primary. Ollama's had no `defaults` while the
    /// other three did, so it was the one built-in that named no model to use.
    #[test]
    fn every_builtin_names_a_primary_model() {
        for (key, config) in &Registry::builtin().providers {
            let primary = config.defaults.primary.as_deref().unwrap_or_default();
            assert!(config.models.contains_key(primary), "{key} names no primary model");
        }
    }

    #[test]
    fn builtin_auth_matches_what_each_format_wants() {
        let registry = Registry::builtin();
        // These three genuinely disagree, and getting it wrong is a 401.
        assert!(matches!(
            registry.get("anthropic").unwrap().auth,
            Auth::ApiKey { ref header, .. } if header == "x-api-key"
        ));
        assert!(matches!(
            registry.get("openai").unwrap().auth,
            Auth::ApiKey { ref header, .. } if header == "Authorization"
        ));
        assert!(matches!(
            registry.get("gemini").unwrap().auth,
            Auth::ApiKey { ref header, .. } if header == "x-goog-api-key"
        ));
    }

    #[test]
    fn a_provider_slash_model_reference_resolves() {
        let registry = registry_with(&[("local", LOCAL)]);
        let model = registry.resolve("local/small").unwrap();
        assert_eq!(model.model_id, "s");
        assert_eq!(model.reference, "local/small");
    }

    #[test]
    fn a_bare_provider_name_uses_its_primary() {
        let registry = registry_with(&[("local", LOCAL)]);
        assert_eq!(registry.resolve("local").unwrap().key, "big");
    }

    #[test]
    fn a_unique_bare_model_key_resolves() {
        let registry = registry_with(&[("local", LOCAL)]);
        assert_eq!(registry.resolve("small").unwrap().reference, "local/small");
    }

    #[test]
    fn an_ambiguous_bare_key_lists_the_candidates() {
        let registry = registry_with(&[("a", LOCAL), ("b", LOCAL)]);
        let error = registry.resolve("small").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("ambiguous"), "{message}");
        assert!(message.contains("a/small") && message.contains("b/small"));
    }

    #[test]
    fn a_model_id_containing_a_slash_is_not_mistaken_for_a_reference() {
        // `anthropic/claude-sonnet-4.5` is a *model id* at OpenRouter, and the
        // provider key there is `openrouter`.
        let registry = registry_with(&[(
            "openrouter",
            r#"{
                "api": "openai-completion", "baseUrl": "https://openrouter.ai/api/v1",
                "models": { "sonnet": { "id": "anthropic/claude-sonnet-4.5" } }
            }"#,
        )]);
        assert_eq!(registry.resolve("openrouter/sonnet").unwrap().model_id, "anthropic/claude-sonnet-4.5");
    }

    #[test]
    fn an_unknown_reference_names_itself() {
        let registry = registry_with(&[("local", LOCAL)]);
        assert!(registry.resolve("nope/nothing").unwrap_err().to_string().contains("nope"));
    }

    #[test]
    fn providers_without_credentials_are_not_offered() {
        // Listing something that will 401 on use is worse than not listing it.
        let registry = registry_with(&[(
            "needs-key",
            r#"{
                "api": "anthropic", "baseUrl": "https://x/v1",
                "auth": { "type": "apiKey", "value": "${OCTANE_DEFINITELY_UNSET_XYZ}" },
                "models": { "m": { "id": "m" } }
            }"#,
        )]);
        assert!(registry.available().is_empty());
        assert!(registry.models().is_empty());
    }

    #[test]
    fn an_unusable_provider_explains_itself() {
        // Vanishing without a word is how someone spends an hour on an unset
        // variable they cannot see.
        let registry = registry_with(&[(
            "needs-key",
            r#"{
                "api": "anthropic", "baseUrl": "https://x/v1",
                "auth": { "type": "apiKey", "value": "${OCTANE_DEFINITELY_UNSET_XYZ}" },
                "models": { "m": { "id": "m" } }
            }"#,
        )]);

        let unavailable = registry.unavailable();
        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0].0, "needs-key");
        assert!(unavailable[0].1.contains("OCTANE_DEFINITELY_UNSET_XYZ"));
    }

    #[test]
    fn a_usable_provider_is_not_listed_as_unavailable() {
        let registry = registry_with(&[("local", LOCAL)]);
        assert!(registry.unavailable().is_empty());
    }

    #[test]
    fn local_providers_need_no_credentials_to_be_offered() {
        let registry = registry_with(&[("local", LOCAL)]);
        assert_eq!(registry.available().len(), 1);
        assert_eq!(registry.models().len(), 2);
    }

    #[test]
    fn a_disabled_provider_is_skipped_without_deleting_it() {
        let registry = registry_with(&[(
            "off",
            r#"{ "api": "anthropic", "baseUrl": "https://x/v1", "disabled": true,
                 "models": { "m": { "id": "m" } } }"#,
        )]);
        assert!(registry.available().is_empty());
        // ...but it is still there to re-enable.
        assert!(registry.get("off").is_some());
    }

    #[test]
    fn roles_resolve_against_a_named_provider() {
        let registry = registry_with(&[("local", LOCAL)]);
        assert_eq!(registry.resolve_role(Some("local"), Role::Primary).unwrap().key, "big");
    }

    #[test]
    fn roles_fall_back_to_the_first_usable_provider() {
        let registry = registry_with(&[("local", LOCAL)]);
        assert_eq!(registry.resolve_role(None, Role::Primary).unwrap().provider, "local");
    }

    #[test]
    fn a_malformed_file_is_reported_rather_than_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let providers = dir.path().join(PROVIDERS_DIR);
        std::fs::create_dir_all(&providers).unwrap();
        std::fs::write(providers.join("broken.json"), "{ not json").unwrap();
        std::fs::write(
            providers.join("good.json"),
            r#"{ "api": "openai-completion", "baseUrl": "http://localhost/v1",
                 "models": { "m": { "id": "m" } } }"#,
        )
        .unwrap();

        let registry = Registry::load(&[dir.path().to_path_buf()]);

        // One bad file must not take the session down.
        assert!(registry.get("good").is_some());
        assert_eq!(registry.errors().len(), 1);
        assert!(registry.errors()[0].to_string().contains("broken.json"));
    }

    #[test]
    fn a_file_replaces_a_builtin_of_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let providers = dir.path().join(PROVIDERS_DIR);
        std::fs::create_dir_all(&providers).unwrap();
        std::fs::write(
            providers.join("anthropic.json"),
            r#"{ "api": "anthropic", "baseUrl": "https://my-proxy/v1",
                 "auth": { "type": "none" },
                 "models": { "mine": { "id": "custom" } } }"#,
        )
        .unwrap();

        let registry = Registry::load(&[dir.path().to_path_buf()]);
        let anthropic = registry.get("anthropic").unwrap();

        // Replaced wholesale, not merged — a half-overridden connection is
        // harder to reason about, and merging makes removal impossible.
        assert_eq!(anthropic.models.len(), 1);
        assert!(anthropic.models.contains_key("mine"));
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let registry = Registry::load(&[std::path::PathBuf::from("/nonexistent/octane")]);
        assert!(registry.errors().is_empty());
        assert!(registry.get("anthropic").is_some(), "builtins survive");
    }

    #[test]
    fn later_roots_win() {
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        for (dir, model) in [(&user, "from-user"), (&project, "from-project")] {
            let providers = dir.path().join(PROVIDERS_DIR);
            std::fs::create_dir_all(&providers).unwrap();
            std::fs::write(
                providers.join("shared.json"),
                format!(
                    r#"{{ "api": "openai-completion", "baseUrl": "http://x/v1",
                          "models": {{ "{model}": {{ "id": "m" }} }} }}"#
                ),
            )
            .unwrap();
        }

        let registry =
            Registry::load(&[user.path().to_path_buf(), project.path().to_path_buf()]);
        assert!(registry.get("shared").unwrap().models.contains_key("from-project"));
    }
}
