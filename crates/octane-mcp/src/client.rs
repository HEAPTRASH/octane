//! The client: lifecycle, then tool discovery and invocation.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;

use crate::protocol::{PROTOCOL_VERSION, Request, Response, negotiate};
use crate::transport::Transport;
use crate::McpError;

/// Default per-request timeout.
///
/// Required, not optional: a server that accepts a request and never answers
/// would otherwise hang the agent loop with no diagnostic.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerCapabilities {
    #[serde(default)]
    pub tools: Option<Capability>,
    #[serde(default)]
    pub resources: Option<Capability>,
    #[serde(default)]
    pub prompts: Option<Capability>,
    #[serde(default)]
    pub logging: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Capability {
    /// Server will notify when its list changes.
    #[serde(default, rename = "listChanged")]
    pub list_changed: bool,
    /// Per-item change subscriptions. Resources only.
    #[serde(default)]
    pub subscribe: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

pub struct McpClient {
    /// Server name as configured, used to namespace tools and to form
    /// `mcp(server/tool)` permission resources.
    server_name: String,
    transport: Arc<dyn Transport>,
    next_id: AtomicU64,
    capabilities: tokio::sync::RwLock<Option<ServerCapabilities>>,
    /// Server-supplied guidance. Untrusted; see [`McpClient::rendered_instructions`].
    instructions: tokio::sync::RwLock<Option<String>>,
    timeout: Duration,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("server_name", &self.server_name)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl McpClient {
    pub fn new(server_name: impl Into<String>, transport: Arc<dyn Transport>) -> Self {
        Self {
            server_name: server_name.into(),
            transport,
            next_id: AtomicU64::new(1),
            capabilities: tokio::sync::RwLock::new(None),
            instructions: tokio::sync::RwLock::new(None),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Perform the `initialize` handshake.
    pub async fn initialize(&self) -> Result<ServerCapabilities, McpError> {
        let params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                // Neither is implemented yet, so neither is advertised. Claiming a
                // capability we cannot honour invites requests we must then fail.
            },
            "clientInfo": { "name": "octane", "version": env!("CARGO_PKG_VERSION") }
        });

        let result = self.call("initialize", Some(params)).await?;

        let server_version = result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpError::Malformed("initialize result lacks protocolVersion".into()))?;
        negotiate(server_version)?;

        let capabilities: ServerCapabilities = result
            .get("capabilities")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| McpError::Malformed(error.to_string()))?
            .unwrap_or_default();

        if let Some(text) = result.get("instructions").and_then(|v| v.as_str()) {
            *self.instructions.write().await = Some(text.to_string());
        }
        *self.capabilities.write().await = Some(capabilities.clone());

        // Only after this may the server send anything but ping and logging.
        self.notify("notifications/initialized", None).await?;

        Ok(capabilities)
    }

    /// List the server's tools.
    pub async fn list_tools(&self) -> Result<Vec<ToolDescriptor>, McpError> {
        // Scoped so the read guard is released before the await below. The
        // shadowed-`drop` version of this looked equivalent but dropped a
        // reference, holding the lock across the request.
        {
            let capabilities = self.capabilities.read().await;
            let capabilities = capabilities.as_ref().ok_or(McpError::NotInitialized)?;
            if capabilities.tools.is_none() {
                return Err(McpError::CapabilityUnavailable("tools"));
            }
        }

        let result = self.call("tools/list", None).await?;
        let tools = result
            .get("tools")
            .cloned()
            .ok_or_else(|| McpError::Malformed("tools/list result lacks tools".into()))?;

        serde_json::from_value(tools).map_err(|error| McpError::Malformed(error.to_string()))
    }

    /// Invoke a tool.
    ///
    /// Authorization is **not** performed here. The caller resolves
    /// `mcp(server/tool)` through the policy engine first — see
    /// [`crate::tool::McpTool::required_permissions`]. Keeping the check out of the
    /// transport means there is exactly one place permissions are decided.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, McpError> {
        let params = serde_json::json!({ "name": name, "arguments": arguments });
        let result = self.call("tools/call", Some(params)).await?;

        // Content is an array of typed blocks. Text blocks are concatenated;
        // anything else is summarized rather than dumped, since a base64 image
        // in a tool result would silently consume the context window.
        let blocks = result.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default();

        let mut out = String::new();
        for block in blocks {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        out.push_str(text);
                        out.push('\n');
                    }
                }
                Some(other) => out.push_str(&format!("[{other} content omitted]\n")),
                None => {}
            }
        }

        // `isError` is the protocol's way of reporting a tool-level failure, as
        // distinct from an RPC error. It is passed through as content so the model
        // can react, which is the whole point of the distinction.
        if result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Ok(format!("Tool reported an error:\n{}", out.trim_end()));
        }

        Ok(out.trim_end().to_string())
    }

    /// Server `instructions`, fenced and attributed.
    ///
    /// Returns `None` when the server supplied none. Never inlined bare: this is
    /// text from a third-party process, and the model must be able to tell it
    /// apart from harness instructions. Fencing is the cheapest way to say
    /// "guidance from this server", not "a rule from octane".
    pub async fn rendered_instructions(&self) -> Option<String> {
        let instructions = self.instructions.read().await;
        instructions.as_ref().map(|text| {
            format!(
                "<mcp-server-instructions server=\"{}\" trust=\"third-party\">\n{}\n</mcp-server-instructions>",
                self.server_name,
                text.trim()
            )
        })
    }

    pub async fn shutdown(&self) -> Result<(), McpError> {
        self.transport.shutdown().await
    }

    async fn call(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = Request::call(id, method, params);
        let encoded =
            serde_json::to_string(&request).map_err(|e| McpError::Malformed(e.to_string()))?;

        self.transport.send(&encoded).await?;

        let receive = async {
            loop {
                let Some(line) = self.transport.recv().await? else {
                    return Err(McpError::Transport("server closed the connection".into()));
                };
                if line.is_empty() {
                    continue;
                }

                let response: Response = serde_json::from_str(&line)
                    .map_err(|error| McpError::Malformed(format!("{error}: {line}")))?;

                // Notifications and replies to other requests are skipped rather
                // than mistaken for ours.
                if response.is_notification() || response.id != Some(id) {
                    continue;
                }
                return response.into_result();
            }
        };

        tokio::time::timeout(self.timeout, receive)
            .await
            .map_err(|_| McpError::Timeout(self.timeout))?
    }

    async fn notify(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), McpError> {
        let request = Request::notify(method, params);
        let encoded =
            serde_json::to_string(&request).map_err(|e| McpError::Malformed(e.to_string()))?;
        self.transport.send(&encoded).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// A scripted peer, so the lifecycle is testable without a server binary.
    struct FakeTransport {
        sent: Mutex<Vec<String>>,
        replies: Mutex<std::collections::VecDeque<String>>,
    }

    impl FakeTransport {
        fn new(replies: Vec<&str>) -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                replies: Mutex::new(replies.iter().map(|s| s.to_string()).collect()),
            })
        }

        fn sent_methods(&self) -> Vec<String> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .filter_map(|raw| {
                    serde_json::from_str::<serde_json::Value>(raw)
                        .ok()?
                        .get("method")?
                        .as_str()
                        .map(str::to_string)
                })
                .collect()
        }
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn send(&self, message: &str) -> Result<(), McpError> {
            self.sent.lock().unwrap().push(message.to_string());
            Ok(())
        }
        async fn recv(&self) -> Result<Option<String>, McpError> {
            Ok(self.replies.lock().unwrap().pop_front())
        }
        async fn shutdown(&self) -> Result<(), McpError> {
            Ok(())
        }
    }

    const INIT_OK: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"s","version":"1"},"instructions":"Prefer the batch tool."}}"#;

    #[tokio::test]
    async fn handshake_negotiates_then_sends_initialized() {
        let transport = FakeTransport::new(vec![INIT_OK]);
        let client = McpClient::new("linter", transport.clone());

        let capabilities = client.initialize().await.unwrap();
        assert!(capabilities.tools.is_some());

        // Order matters: initialize, then the notification.
        assert_eq!(transport.sent_methods(), vec!["initialize", "notifications/initialized"]);
    }

    #[tokio::test]
    async fn an_unsupported_version_fails_rather_than_proceeding() {
        let reply = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"1999-01-01","capabilities":{}}}"#;
        let client = McpClient::new("s", FakeTransport::new(vec![reply]));

        let error = client.initialize().await.unwrap_err();
        assert!(matches!(error, McpError::UnsupportedProtocol { .. }));
    }

    #[tokio::test]
    async fn listing_tools_before_initialize_is_refused() {
        let client = McpClient::new("s", FakeTransport::new(vec![]));
        assert!(matches!(client.list_tools().await.unwrap_err(), McpError::NotInitialized));
    }

    #[tokio::test]
    async fn a_server_without_the_tools_capability_is_not_queried() {
        let reply = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{}}}"#;
        let client = McpClient::new("s", FakeTransport::new(vec![reply]));
        client.initialize().await.unwrap();

        let error = client.list_tools().await.unwrap_err();
        assert!(matches!(error, McpError::CapabilityUnavailable("tools")));
    }

    #[tokio::test]
    async fn interleaved_notifications_do_not_satisfy_a_request() {
        let transport = FakeTransport::new(vec![
            r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#,
            INIT_OK,
        ]);
        let client = McpClient::new("s", transport);

        // The notification is skipped and the real reply is matched by id.
        assert!(client.initialize().await.is_ok());
    }

    #[tokio::test]
    async fn text_content_blocks_are_concatenated() {
        let transport = FakeTransport::new(vec![
            INIT_OK,
            r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"line one"},{"type":"text","text":"line two"}]}}"#,
        ]);
        let client = McpClient::new("s", transport);
        client.initialize().await.unwrap();

        let output = client.call_tool("check", serde_json::json!({})).await.unwrap();
        assert_eq!(output, "line one\nline two");
    }

    #[tokio::test]
    async fn binary_content_is_summarized_not_inlined() {
        let transport = FakeTransport::new(vec![
            INIT_OK,
            r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"image","data":"AAAA"}]}}"#,
        ]);
        let client = McpClient::new("s", transport);
        client.initialize().await.unwrap();

        let output = client.call_tool("snap", serde_json::json!({})).await.unwrap();
        // The base64 must not reach the context window.
        assert!(!output.contains("AAAA"));
        assert!(output.contains("image content omitted"));
    }

    #[tokio::test]
    async fn tool_level_errors_reach_the_model() {
        let transport = FakeTransport::new(vec![
            INIT_OK,
            r#"{"jsonrpc":"2.0","id":2,"result":{"isError":true,"content":[{"type":"text","text":"file not found"}]}}"#,
        ]);
        let client = McpClient::new("s", transport);
        client.initialize().await.unwrap();

        let output = client.call_tool("read", serde_json::json!({})).await.unwrap();
        assert!(output.contains("file not found"));
        assert!(output.contains("error"));
    }

    #[tokio::test]
    async fn server_instructions_are_fenced_as_untrusted() {
        let client = McpClient::new("linter", FakeTransport::new(vec![INIT_OK]));
        client.initialize().await.unwrap();

        let rendered = client.rendered_instructions().await.unwrap();
        assert!(rendered.contains(r#"trust="third-party""#));
        assert!(rendered.contains(r#"server="linter""#));
        assert!(rendered.contains("Prefer the batch tool."));
    }

    #[tokio::test]
    async fn no_instructions_renders_nothing() {
        let reply = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}}}}"#;
        let client = McpClient::new("s", FakeTransport::new(vec![reply]));
        client.initialize().await.unwrap();
        assert!(client.rendered_instructions().await.is_none());
    }
}
