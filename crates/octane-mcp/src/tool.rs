//! Adapting an MCP tool to the local [`Tool`] interface.
//!
//! Once wrapped, an MCP tool is indistinguishable from a built-in: same registry,
//! same permission engine, same execution pipeline. That is the property that
//! makes MCP safe to support — there is no second, weaker path into tool
//! execution.

use std::sync::Arc;

use async_trait::async_trait;
use octane_tools::{Tool, ToolContext, ToolError, ToolOutcome};

use crate::client::{McpClient, ToolDescriptor};

pub struct McpTool {
    client: Arc<McpClient>,
    /// Name on the server.
    remote_name: String,
    /// Name in our registry, namespaced so two servers offering `search` do not
    /// collide and so the user can tell where a tool came from.
    local_name: String,
    description: String,
    input_schema: serde_json::Value,
}

impl McpTool {
    pub fn new(client: Arc<McpClient>, descriptor: ToolDescriptor) -> Self {
        let local_name = format!("mcp__{}__{}", client.server_name(), descriptor.name);
        let description = descriptor.description.unwrap_or_else(|| {
            format!("Tool {:?} provided by MCP server {:?}.", descriptor.name, client.server_name())
        });

        Self {
            client,
            remote_name: descriptor.name,
            local_name,
            description,
            input_schema: descriptor.input_schema,
        }
    }
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool")
            .field("local_name", &self.local_name)
            .field("remote_name", &self.remote_name)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.local_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    fn required_permissions(&self, _input: &str, _ctx: &ToolContext) -> Vec<String> {
        // `mcp(server/tool)`, so a user can allow a whole server with
        // `mcp(linter/*)` or a single tool with `mcp(sql/execute)`.
        vec![format!("mcp({}/{})", self.client.server_name(), self.remote_name)]
    }

    fn is_mutating(&self) -> bool {
        // Conservative: an MCP tool's effects are opaque to us, and a schema
        // cannot be trusted to reveal them. Treating every one as mutating means
        // a `plan` agent never invokes one, which is the right default even
        // though it excludes read-only MCP tools too.
        true
    }

    async fn execute(&self, input: &str, ctx: &ToolContext) -> Result<ToolOutcome, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        // Empty input is normal for zero-argument tools; `{}` keeps the server
        // from having to special-case it.
        let arguments: serde_json::Value = if input.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(input)
                .map_err(|error| ToolError::Recoverable(format!("arguments are not valid JSON: {error}")))?
        };

        match self.client.call_tool(&self.remote_name, arguments).await {
            Ok(output) => Ok(ToolOutcome::new(self.local_name.clone(), output)),
            // Reported as recoverable so the model can adjust its arguments or
            // give up gracefully; a failing MCP server should not kill the turn.
            Err(error) => Err(ToolError::Recoverable(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Transport;
    use octane_protocol::{SessionId, ToolCallId};

    struct DeadTransport;

    #[async_trait]
    impl Transport for DeadTransport {
        async fn send(&self, _message: &str) -> Result<(), crate::McpError> {
            Ok(())
        }
        async fn recv(&self) -> Result<Option<String>, crate::McpError> {
            Ok(None)
        }
        async fn shutdown(&self) -> Result<(), crate::McpError> {
            Ok(())
        }
    }

    fn tool() -> McpTool {
        let client = Arc::new(McpClient::new("linter", Arc::new(DeadTransport)));
        McpTool::new(
            client,
            ToolDescriptor {
                name: "check".into(),
                description: Some("Lints a file.".into()),
                input_schema: serde_json::json!({"type": "object"}),
            },
        )
    }

    fn context() -> ToolContext {
        ToolContext {
            session_id: SessionId::new(),
            call_id: ToolCallId::new(),
            agent: "build".into(),
            workspace: "/p".into(),
            cwd: "/p".into(),
            cancel: Default::default(),
        }
    }

    #[test]
    fn local_names_are_namespaced_by_server() {
        assert_eq!(tool().name(), "mcp__linter__check");
    }

    #[test]
    fn permission_resource_is_server_slash_tool() {
        let permissions = tool().required_permissions("{}", &context());
        assert_eq!(permissions, vec!["mcp(linter/check)"]);

        // And it parses as a Resource the policy engine understands.
        use octane_permission::{Action, Resource};
        let parsed: Resource = permissions[0].parse().unwrap();
        assert_eq!(parsed.action, Action::Mcp);
        assert_eq!(parsed.target, "linter/check");
    }

    #[test]
    fn mcp_tools_are_treated_as_mutating() {
        assert!(tool().is_mutating(), "effects are opaque, so assume the worst");
    }

    #[tokio::test]
    async fn malformed_arguments_are_recoverable() {
        let error = tool().execute("not json", &context()).await.unwrap_err();
        assert!(matches!(error, ToolError::Recoverable(_)));
        assert!(error.is_reportable_to_model());
    }

    #[tokio::test]
    async fn cancellation_is_checked_before_dispatch() {
        let ctx = context();
        ctx.cancel.cancel();
        assert!(matches!(tool().execute("{}", &ctx).await.unwrap_err(), ToolError::Cancelled));
    }
}
