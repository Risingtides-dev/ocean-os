//! Observatory payloads are allow-listed types: no runtime event or raw payload is serializable here.
//! Keep free-form values restricted to safe labels and fixed reason/error codes at adapter boundaries.
use crate::EventPayload;
#[derive(Debug, thiserror::Error)]
pub enum RedactionError {
    #[error("event has no safe observatory projection")]
    Unsupported,
}
pub fn serialize_safe(payload: &EventPayload) -> Result<String, serde_json::Error> {
    serde_json::to_string(payload)
}
