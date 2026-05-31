//! JSON-RPC 2.0 message types, as used by MCP over any transport.
//!
//! Only the subset MCP needs: client→server requests and notifications, and
//! server→client responses. We don't model server→client requests (sampling,
//! roots) because this client doesn't advertise those capabilities yet.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";

/// A JSON-RPC request carrying a numeric id. MCP ids may be string or number;
/// we always send numbers and match responses by the number we sent.
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

/// A JSON-RPC notification — a request with no id, expecting no response.
#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub jsonrpc: &'static str,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
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

/// A message read from the server. The stdio transport reads one JSON object
/// per line; it may be a response to one of our requests (has `id`) or a
/// server-initiated notification (has `method`, no `id`). We only act on
/// responses; notifications are logged and dropped for now.
#[derive(Debug, Clone, Deserialize)]
pub struct Incoming {
    #[allow(dead_code)]
    pub jsonrpc: Option<String>,
    /// JSON-RPC id, kept as a raw `Value` so we match both numeric ids (what we
    /// send) and string ids (which some real servers echo back). Modeling this
    /// as `Option<u64>` would deserialize a string id to `None`, misclassify the
    /// response as a notification, and hang the request until timeout.
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
    /// True when this is a response (carries an id) rather than a
    /// server-initiated notification. Discriminating on `id` alone is the
    /// spec-correct test: a response MUST have an id, a notification MUST NOT —
    /// `method` may also be present on some non-strict servers' responses, so we
    /// don't gate on its absence.
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
    fn string_id_response_still_matches_and_is_a_response() {
        // Some real servers echo the id as a string. Must not be misclassified
        // as a notification (which would hang the request until timeout).
        let msg = parse(r#"{"jsonrpc":"2.0","id":"7","result":{}}"#);
        assert!(msg.is_response(), "string-id message is a response");
        assert!(msg.matches_id(7), "string id matches the numeric id we sent");
    }

    #[test]
    fn notification_is_not_a_response() {
        let msg = parse(r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#);
        assert!(!msg.is_response());
    }

    #[test]
    fn response_with_method_present_is_still_a_response() {
        // Non-strict servers sometimes echo `method` on responses; the id is the
        // discriminator, not the absence of method.
        let msg = parse(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","result":{}}"#);
        assert!(msg.is_response());
        assert!(msg.matches_id(3));
    }
}
