//! MCP client — third-party tools, spoken over JSON-RPC 2.0.
//!
//! Three lifecycle phases, per the
//! [spec](https://modelcontextprotocol.io/specification/latest/basic/lifecycle):
//!
//! 1. **Initialize.** Client sends `initialize` with its protocol version,
//!    capabilities, and `clientInfo`. Must not be batched. Server replies with its
//!    capabilities, `serverInfo`, and optional `instructions`. Client then sends
//!    `notifications/initialized`. Nothing but `ping` may precede the response.
//! 2. **Operation.** Only negotiated capabilities may be used.
//! 3. **Shutdown.** No shutdown message exists. For stdio: close the child's
//!    stdin, wait, then `SIGTERM`.
//!
//! Two integration rules this crate exists to uphold (`RESEARCH.md` §C):
//!
//! - MCP tools enter the **same registry** and the **same permission engine** as
//!   built-ins. From the model's side they are indistinguishable; from the user's
//!   side they are equally governed. That is what makes adding a server a bounded
//!   decision.
//! - The tool list must be **sorted** before it reaches the prompt. `ToolRegistry`
//!   guarantees this, and it is why Codex's non-deterministic MCP ordering bug
//!   cannot recur here.
//!
//! Server-supplied `instructions` are **untrusted third-party text**. They are
//! fenced and marked as such, never spliced into the system prompt as though the
//! harness had authored them.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod client;
pub mod protocol;
pub mod tool;
pub mod transport;

pub use client::{McpClient, ServerCapabilities};
pub use protocol::{PROTOCOL_VERSION, Request, Response};
pub use tool::McpTool;
pub use transport::{StdioTransport, Transport};

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("transport failure: {0}")]
    Transport(String),

    #[error("server speaks protocol {server_version}, which this client does not support")]
    UnsupportedProtocol { server_version: String },

    #[error("server returned error {code}: {message}")]
    Rpc { code: i64, message: String },

    #[error("malformed message: {0}")]
    Malformed(String),

    #[error("capability {0:?} was not negotiated")]
    CapabilityUnavailable(&'static str),

    #[error("request timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("server is not initialized")]
    NotInitialized,
}
