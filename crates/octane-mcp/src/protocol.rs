//! JSON-RPC 2.0 message shapes.

use serde::{Deserialize, Serialize};

/// Protocol version this client advertises.
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// Versions this client can still speak if a server negotiates downwards.
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

#[derive(Debug, Clone, Serialize)]
pub struct Request {
    pub jsonrpc: &'static str,
    /// Absent for notifications, which take no reply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl Request {
    pub fn call(id: u64, method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self { jsonrpc: "2.0", id: Some(id), method: method.into(), params }
    }

    pub fn notify(method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self { jsonrpc: "2.0", id: None, method: method.into(), params }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Response {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<u64>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
    /// Present on server-initiated notifications, which share the envelope.
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

impl Response {
    pub fn is_notification(&self) -> bool {
        self.id.is_none() && self.method.is_some()
    }

    pub fn into_result(self) -> Result<serde_json::Value, crate::McpError> {
        if let Some(error) = self.error {
            return Err(crate::McpError::Rpc { code: error.code, message: error.message });
        }
        self.result
            .ok_or_else(|| crate::McpError::Malformed("response has neither result nor error".into()))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub data: Option<serde_json::Value>,
}

/// Negotiate a version against what this client supports.
///
/// The spec says a client that cannot speak the server's chosen version SHOULD
/// disconnect. Continuing regardless would mean sending messages the server may
/// interpret differently, which is worse than a clear failure.
pub fn negotiate(server_version: &str) -> Result<String, crate::McpError> {
    if SUPPORTED_VERSIONS.contains(&server_version) {
        return Ok(server_version.to_string());
    }
    Err(crate::McpError::UnsupportedProtocol { server_version: server_version.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_carry_an_id_and_notifications_do_not() {
        let call = serde_json::to_value(Request::call(1, "initialize", None)).unwrap();
        assert_eq!(call["id"], 1);

        let notify = serde_json::to_value(Request::notify("notifications/initialized", None)).unwrap();
        assert!(notify.get("id").is_none(), "a notification must not carry an id");
    }

    #[test]
    fn accepts_our_own_and_older_versions() {
        assert_eq!(negotiate(PROTOCOL_VERSION).unwrap(), PROTOCOL_VERSION);
        assert_eq!(negotiate("2025-03-26").unwrap(), "2025-03-26");
    }

    #[test]
    fn rejects_an_unknown_version_rather_than_guessing() {
        assert!(negotiate("1999-01-01").is_err());
    }

    #[test]
    fn rpc_errors_surface_as_errors() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"no such method"}}"#;
        let response: Response = serde_json::from_str(raw).unwrap();
        let error = response.into_result().unwrap_err();
        assert!(matches!(error, crate::McpError::Rpc { code: -32601, .. }));
    }

    #[test]
    fn server_notifications_are_distinguishable() {
        let raw = r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;
        let response: Response = serde_json::from_str(raw).unwrap();
        assert!(response.is_notification());
    }
}
