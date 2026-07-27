//! Declaring servers.
//!
//! One `mcp.json` per config root, discovered the same way everything else in
//! octane is: `~/.octane/mcp.json` first, then `.octane/mcp.json`, with a project
//! entry replacing a user entry of the same name.
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "linter": {
//!       "command": "npx",
//!       "args": ["-y", "@example/lint-mcp"],
//!       "env": { "LINT_TOKEN": "${LINT_TOKEN}" }
//!     },
//!     "issues": {
//!       "url": "https://mcp.example.com/mcp",
//!       "headers": { "Authorization": "Bearer ${ISSUES_TOKEN}" }
//!     }
//!   }
//! }
//! ```
//!
//! `command` declares a stdio server and `url` a streamable-HTTP one. Exactly one
//! of them, never both: a declaration naming each has no defensible reading, and
//! picking one silently would connect somewhere the author did not mean.
//!
//! The `mcpServers` key is the shape Claude Code, Cursor and opencode already
//! use. Matching it is worth more than a tidier name: an existing config can be
//! copied across unchanged, and nobody has to learn a second format.
//!
//! **Values are `${VAR}` references, not secrets.** A server almost always needs
//! a token, and this file lives in a repository. Referencing the environment
//! keeps the file committable and the secret out of it — the same trade
//! `octane-provider` makes for API keys, for the same reason.

use std::collections::BTreeMap;
use std::sync::Arc;

use camino::Utf8Path;
use serde::{Deserialize, Serialize};

use crate::transport::{HttpTransport, StdioTransport, Transport};
use crate::McpError;

/// File name looked for under each config root.
pub const CONFIG_FILE: &str = "mcp.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Executable to spawn, for a stdio server. Exclusive with `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    #[serde(default)]
    pub args: Vec<String>,

    /// Applied on top of the inherited environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Endpoint of a streamable-HTTP server. Exclusive with `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Extra request headers, which is where a remote server's auth goes.
    /// `${VAR}` is expanded exactly as in `env`, and for the same reason.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,

    /// Declared but not started. Keeping a server in the file while it is off is
    /// how someone turns one back on without having to remember its arguments.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize)]
struct File {
    #[serde(default, rename = "mcpServers")]
    servers: BTreeMap<String, ServerConfig>,
}

impl ServerConfig {
    /// Resolve `${VAR}` references in `env` values.
    ///
    /// `lookup` is injected rather than read directly because `unsafe_code` is
    /// forbidden workspace-wide, which rules out `std::env::set_var` and so rules
    /// out tests that set a variable to check this.
    ///
    /// An unset variable is an error, not an empty string: a server handed an
    /// empty token fails somewhere inside itself, and the report comes back as
    /// "the server is broken" rather than "you did not export it".
    pub fn resolve_env_with(
        mut self,
        server: &str,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, McpError> {
        for (key, value) in &mut self.env {
            *value = expand(value, server, key, &lookup)?;
        }
        // Headers carry the remote server's bearer token, so they get the same
        // treatment: the file stays committable and the secret stays out of it.
        for (key, value) in &mut self.headers {
            *value = expand(value, server, key, &lookup)?;
        }

        // The url too: query-string auth is common for hosted endpoints, and a
        // reference left unexpanded there would be sent literally — the one
        // field silently exempt from what this module's doc promises.
        if let Some(url) = &mut self.url {
            *url = expand(url, server, "url", &lookup)?;
        }
        Ok(self)
    }

    /// Reject a declaration that names neither transport or both.
    ///
    /// Checked at discovery rather than at connect time so the report names the
    /// server while the file is being read, instead of surfacing later as a spawn
    /// failure for an empty program name.
    pub fn validated(self, server: &str) -> Result<Self, McpError> {
        let reason = match (&self.command, &self.url) {
            (Some(_), None) | (None, Some(_)) => return Ok(self),
            (Some(_), Some(_)) => "declares both command and url; they are exclusive",
            (None, None) => "declares neither command nor url",
        };
        Err(McpError::Config { server: server.to_string(), reason: reason.to_string() })
    }

    /// Open the transport this declaration asks for.
    ///
    /// The stdio/HTTP choice lives here rather than at the call site because
    /// every caller would otherwise repeat it, and a caller that forgot `url`
    /// would quietly ignore every remote server in the file.
    pub async fn connect(&self, server: &str) -> Result<Arc<dyn Transport>, McpError> {
        if let Some(url) = &self.url {
            let transport = HttpTransport::connect(url, &self.headers)?;
            return Ok(Arc::new(transport));
        }

        let command = self
            .command
            .as_deref()
            .ok_or_else(|| McpError::Config {
                server: server.to_string(),
                reason: "declares neither command nor url".into(),
            })?;
        let env: Vec<(String, String)> =
            self.env.iter().map(|(key, value)| (key.clone(), value.clone())).collect();
        Ok(Arc::new(StdioTransport::spawn(command, &self.args, &env).await?))
    }
}

fn expand(
    value: &str,
    server: &str,
    key: &str,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<String, McpError> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // Unterminated: emit literally rather than guessing where it ends.
            out.push_str(&rest[start..]);
            return Ok(out);
        };

        let name = &after[..end];
        let resolved = lookup(name).ok_or_else(|| McpError::Config {
            server: server.to_string(),
            reason: format!("{key} references ${{{name}}}, which is not set"),
        })?;
        out.push_str(&resolved);
        rest = &after[end + 1..];
    }

    out.push_str(rest);
    Ok(out)
}

/// Read every `mcp.json` under `roots`, least specific first.
///
/// A malformed file is reported and skipped rather than fatal. These are files
/// from a repository, and one bad one must not stop a session from starting —
/// the same rule skills and commands follow.
pub fn discover(
    roots: &[impl AsRef<Utf8Path>],
    lookup: impl Fn(&str) -> Option<String> + Copy,
) -> (BTreeMap<String, ServerConfig>, Vec<McpError>) {
    let mut servers = BTreeMap::new();
    let mut errors = Vec::new();

    for root in roots {
        let path = root.as_ref().join(CONFIG_FILE);
        if !path.is_file() {
            continue;
        }

        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) => {
                errors.push(McpError::Config {
                    server: path.to_string(),
                    reason: error.to_string(),
                });
                continue;
            }
        };

        let file: File = match serde_json::from_str(&raw) {
            Ok(file) => file,
            Err(error) => {
                errors.push(McpError::Malformed(format!("{path}: {error}")));
                continue;
            }
        };

        for (name, server) in file.servers {
            match server.resolve_env_with(&name, lookup).and_then(|s| s.validated(&name)) {
                // A later root replaces an earlier one wholesale, so a project
                // can repoint a server without inheriting half of the user's.
                Ok(resolved) => {
                    servers.insert(name, resolved);
                }
                Err(error) => errors.push(error),
            }
        }
    }

    (servers, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    fn temp() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        (dir, path)
    }

    fn write(dir: &Utf8PathBuf, body: &str) {
        std::fs::write(dir.join(CONFIG_FILE), body).unwrap();
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// Query-string auth is common for hosted endpoints, so a reference in the
    /// url must resolve like one in a header rather than being sent literally.
    #[test]
    fn a_reference_in_the_url_is_resolved_too() {
        let (_guard, dir) = temp();
        write(&dir, r#"{"mcpServers":{"h":{"url":"https://x.example/mcp?key=${K}"}}}"#);

        let (servers, errors) =
            discover(&[dir], |name| (name == "K").then(|| "s3cret".to_string()));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(servers["h"].url.as_deref(), Some("https://x.example/mcp?key=s3cret"));
    }

    #[test]
    fn a_server_is_read_from_the_standard_key() {
        let (_guard, dir) = temp();
        write(&dir, r#"{"mcpServers":{"linter":{"command":"npx","args":["-y","lint"]}}}"#);

        let (servers, errors) = discover(&[dir], no_env);
        assert!(errors.is_empty(), "{errors:?}");

        let linter = servers.get("linter").expect("linter");
        assert_eq!(linter.command.as_deref(), Some("npx"));
        assert_eq!(linter.args, ["-y", "lint"]);
        assert!(linter.enabled, "a server is on unless it says otherwise");
    }

    #[test]
    fn a_remote_server_is_declared_by_url_instead_of_command() {
        let (_guard, dir) = temp();
        write(&dir, r#"{"mcpServers":{"issues":{"url":"https://example.test/mcp"}}}"#);

        let (servers, errors) = discover(&[dir], no_env);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(servers["issues"].url.as_deref(), Some("https://example.test/mcp"));
        assert!(servers["issues"].command.is_none());
    }

    /// Both readings are defensible, which is exactly why neither is chosen.
    #[test]
    fn a_server_naming_both_a_command_and_a_url_is_refused_rather_than_guessed() {
        let (_guard, dir) = temp();
        write(&dir, r#"{"mcpServers":{"x":{"command":"c","url":"https://example.test/mcp"}}}"#);

        let (servers, errors) = discover(&[dir], no_env);
        assert!(servers.is_empty(), "an ambiguous declaration must not connect");
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn a_server_naming_no_transport_at_all_is_refused() {
        let (_guard, dir) = temp();
        write(&dir, r#"{"mcpServers":{"x":{"args":["--stdio"]}}}"#);

        let (servers, errors) = discover(&[dir], no_env);
        assert!(servers.is_empty());
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    /// Same trade as `env`: the file is committable and the token is not in it.
    #[test]
    fn a_header_reference_is_resolved_so_a_token_stays_out_of_the_file() {
        let (_guard, dir) = temp();
        write(
            &dir,
            r#"{"mcpServers":{"x":{"url":"https://e.test/mcp","headers":{"Authorization":"Bearer ${TOK}"}}}}"#,
        );

        let (servers, errors) =
            discover(&[dir], |name| (name == "TOK").then(|| "abc".to_string()));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(servers["x"].headers["Authorization"], "Bearer abc");
    }

    #[test]
    fn an_unset_header_variable_is_an_error_rather_than_an_empty_token() {
        let (_guard, dir) = temp();
        write(
            &dir,
            r#"{"mcpServers":{"x":{"url":"https://e.test/mcp","headers":{"Authorization":"${NOPE}"}}}}"#,
        );

        let (servers, errors) = discover(&[dir], no_env);
        assert!(servers.is_empty(), "a server that cannot authenticate must not connect");
        assert!(errors.iter().any(|e| e.to_string().contains("NOPE")), "{errors:?}");
    }

    #[test]
    fn a_project_server_replaces_a_user_server_of_the_same_name() {
        let (_user_guard, user) = temp();
        let (_project_guard, project) = temp();
        write(&user, r#"{"mcpServers":{"db":{"command":"user-db"}}}"#);
        write(&project, r#"{"mcpServers":{"db":{"command":"project-db"}}}"#);

        let (servers, _) = discover(&[user, project], no_env);
        assert_eq!(servers["db"].command.as_deref(), Some("project-db"));
    }

    #[test]
    fn an_env_reference_is_resolved_from_the_environment() {
        let (_guard, dir) = temp();
        write(&dir, r#"{"mcpServers":{"x":{"command":"c","env":{"TOKEN":"${T}!"}}}}"#);

        let (servers, errors) =
            discover(&[dir], |name| (name == "T").then(|| "secret".to_string()));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(servers["x"].env["TOKEN"], "secret!");
    }

    /// Reported, not silently blanked. A server handed an empty token fails
    /// inside itself, and the user is told the server is broken instead of being
    /// told they forgot to export something.
    #[test]
    fn an_unset_variable_is_an_error_rather_than_an_empty_string() {
        let (_guard, dir) = temp();
        write(&dir, r#"{"mcpServers":{"x":{"command":"c","env":{"TOKEN":"${MISSING}"}}}}"#);

        let (servers, errors) = discover(&[dir], no_env);
        assert!(servers.is_empty(), "a server that cannot be configured must not start");
        assert!(
            errors.iter().any(|e| e.to_string().contains("MISSING")),
            "the error must name the variable: {errors:?}",
        );
    }

    /// The dispatch is silent when it goes wrong — a url routed to the process
    /// path would report a spawn failure — so both branches get a witness.
    #[tokio::test]
    async fn a_url_opens_an_http_transport_while_a_command_starts_a_process() {
        let blank = ServerConfig {
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
        };

        // Never contacted: connecting is deferred to the first message.
        let remote =
            ServerConfig { url: Some("https://example.test/mcp".into()), ..blank.clone() };
        assert!(remote.connect("issues").await.is_ok());

        let local = ServerConfig { command: Some("octane-no-such-program".into()), ..blank };
        // `.err()` rather than `unwrap_err`: `Arc<dyn Transport>` is not `Debug`.
        let error = local.connect("linter").await.err().unwrap().to_string();
        assert!(error.contains("octane-no-such-program"), "{error}");
    }

    #[test]
    fn a_malformed_file_is_reported_and_skipped_rather_than_fatal() {
        let (_guard, dir) = temp();
        write(&dir, "{ not json");

        let (servers, errors) = discover(&[dir], no_env);
        assert!(servers.is_empty());
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn a_disabled_server_is_still_read_so_it_can_be_turned_back_on() {
        let (_guard, dir) = temp();
        write(&dir, r#"{"mcpServers":{"off":{"command":"c","enabled":false}}}"#);

        let (servers, _) = discover(&[dir], no_env);
        assert!(!servers["off"].enabled);
    }
}
