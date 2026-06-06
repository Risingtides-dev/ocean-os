//! JSON-RPC 2.0 message types for the plugin stdio protocol.
//!
//! This mirrors `ocean-mcp`'s `jsonrpc` module (the proven MCP framing) rather
//! than depending on it: the plugin wire is the same newline-delimited JSON-RPC
//! 2.0, so the framing types are the same. We model only what the plugin runtime
//! sends and receives — client→plugin requests, and plugin→client responses /
//! errors. (No server-initiated requests; a plugin doesn't call back into the
//! host.)

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";

/// A JSON-RPC request carrying a numeric id. We always send numeric ids and
/// match responses by the number we sent.
#[derive(Debug, Clone, Serialize)]
pub struct Request {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    pub fn new(id: u64, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

/// A message read from the plugin. The stdio transport reads one JSON object per
/// line; it is a response to one of our requests (has `id`). `id` is kept as a
/// raw `Value` so a plugin that echoes a string id is still matched to the
/// numeric id we sent rather than being misread.
#[derive(Debug, Clone, Deserialize)]
pub struct Incoming {
    #[allow(dead_code)]
    pub jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

impl Incoming {
    /// True when this message carries an id (a response) rather than being a
    /// stray notification line.
    pub fn is_response(&self) -> bool {
        self.id.is_some()
    }

    /// Whether this message's id matches the numeric id we sent (accepting a
    /// string-encoded id too).
    pub fn matches_id(&self, sent: u64) -> bool {
        match &self.id {
            Some(Value::Number(n)) => n.as_u64() == Some(sent),
            Some(Value::String(s)) => s.parse::<u64>().ok() == Some(sent),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Incoming {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn numeric_id_response_matches() {
        let msg = parse(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#);
        assert!(msg.is_response());
        assert!(msg.matches_id(7));
        assert!(!msg.matches_id(8));
    }

    #[test]
    fn string_id_response_still_matches() {
        let msg = parse(r#"{"jsonrpc":"2.0","id":"7","result":{}}"#);
        assert!(msg.is_response());
        assert!(msg.matches_id(7));
    }

    #[test]
    fn error_response_parses() {
        let msg = parse(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"nope"}}"#);
        assert!(msg.is_response());
        let e = msg.error.unwrap();
        assert_eq!(e.code, -32601);
        assert_eq!(e.message, "nope");
    }
}
