//! Ephemeral OpenAI Realtime session support (voice phases 2/3).
//!
//! `POST /v1/voice/realtime/client-secret` lets a surface start a realtime
//! voice chat without ever holding a provider key: the daemon resolves the
//! OpenAI credential (env / auth file via `ocean-providers`), mints a
//! short-lived client secret upstream with the session briefing + voice-agent
//! tools baked in, and returns `{ client_secret, expires_at, model }`. The
//! browser then talks WebRTC directly to OpenAI with that secret.
//!
//! The handler glue lives in `main.rs`; this module owns the pure pieces
//! (briefing builder, upstream body, response normalization) so they stay
//! unit-testable without HTTP.

use serde::Deserialize;
use serde_json::{json, Value};

/// Default Realtime model — current public id; the surface may override
/// per-request (e.g. the cheaper mini variant) via the request body.
pub(crate) const DEFAULT_REALTIME_MODEL: &str = "gpt-realtime-2";

/// Upstream mint endpoint (GA Realtime API).
const UPSTREAM_URL: &str = "https://api.openai.com/v1/realtime/client_secrets";

/// Ephemeral secret lifetime. Long enough to cover a voice session's WebRTC
/// handshake with slack; the WebRTC session itself outlives the secret.
const SECRET_TTL_SECS: u32 = 600;

/// Briefing caps: newest-last transcript tail, bounded both by entry count
/// and total characters so a long session can never blow the instructions.
const BRIEFING_MAX_ENTRIES: usize = 30;
const BRIEFING_CHAR_BUDGET: usize = 8_000;

#[derive(Debug, Deserialize)]
pub(crate) struct RealtimeSecretRequest {
    /// Chat session to brief the voice agent on (and the target of its
    /// `write_handoff` notes). Optional — a session-less voice chat gets the
    /// header-only instructions.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Realtime model override; defaults to [`DEFAULT_REALTIME_MODEL`].
    #[serde(default)]
    pub model: Option<String>,
}

/// Render the voice agent's instructions: a fixed header describing the two
/// tools, plus a compact `role: text` tail of the chat transcript when one
/// was supplied. Newest entries win the budget (we keep the tail, not the
/// head) since they carry the live context.
pub(crate) fn build_instructions(transcript: &[(String, String)]) -> String {
    let mut out = String::from(
        "You are Ocean's realtime voice agent. Converse naturally and briefly. \
         You have two tools: `render_component` renders an interactive UI \
         component on the user's surface (kanban, form, table, chart, ...); \
         `write_handoff` leaves a task note in the chat session so the text \
         agent picks it up for real coding work — use it whenever the user \
         asks for non-trivial code or file changes.",
    );
    let tail: Vec<&(String, String)> = transcript.iter().rev().take(BRIEFING_MAX_ENTRIES).collect();
    if tail.is_empty() {
        return out;
    }
    out.push_str("\n\nCurrent chat session (oldest first):\n");
    let mut lines: Vec<String> = Vec::with_capacity(tail.len());
    let mut used = 0usize;
    // Walk newest→oldest, keep entries while they fit, then restore order.
    for (role, text) in &tail {
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let line = format!("{role}: {text}\n");
        if used + line.len() > BRIEFING_CHAR_BUDGET {
            break;
        }
        used += line.len();
        lines.push(line);
    }
    for line in lines.iter().rev() {
        out.push_str(line);
    }
    out
}

/// The upstream mint body: TTL + a realtime session config carrying the
/// model, instructions, output voice, and the two voice-agent tools.
pub(crate) fn upstream_body(model: &str, instructions: &str) -> Value {
    json!({
        "expires_after": { "anchor": "created_at", "seconds": SECRET_TTL_SECS },
        "session": {
            "type": "realtime",
            "model": model,
            "instructions": instructions,
            "audio": { "output": { "voice": "marin" } },
            "tools": [
                {
                    "type": "function",
                    "name": "render_component",
                    "description": "Render an interactive UI component on the user's surface. Pass the component JSON the Ocean surface understands.",
                    // Deliberately permissive: the surface validates/renders,
                    // the daemon does not gatekeep component shapes here.
                    "parameters": { "type": "object" }
                },
                {
                    "type": "function",
                    "name": "write_handoff",
                    "description": "Leave a task note in the chat session for the text agent to pick up (real coding work, follow-ups).",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "note": { "type": "string", "description": "The task note." }
                        },
                        "required": ["note"]
                    }
                }
            ]
        }
    })
}

/// Normalize the upstream mint response to the frozen surface contract
/// `{ client_secret, expires_at, model }`. GA returns the secret at `value`;
/// tolerate a nested `client_secret.value` for forward/backward drift.
pub(crate) fn normalize_upstream(upstream: &Value, model: &str) -> Result<Value, String> {
    let secret = upstream
        .pointer("/value")
        .or_else(|| upstream.pointer("/client_secret/value"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "upstream mint response carried no client secret".to_string())?;
    let expires_at = upstream
        .pointer("/expires_at")
        .or_else(|| upstream.pointer("/client_secret/expires_at"))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(json!({
        "client_secret": secret,
        "expires_at": expires_at,
        "model": model,
    }))
}

/// POST the mint body upstream with the resolved API key. Returns the
/// normalized contract JSON or a human-readable error (mapped to 502 by the
/// handler — the key itself never appears in errors).
pub(crate) async fn mint_client_secret(
    api_key: &str,
    model: &str,
    body: &Value,
) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client build failed: {e}"))?;
    let resp = client
        .post(UPSTREAM_URL)
        .bearer_auth(api_key)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("upstream mint request failed: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("upstream mint response unreadable: {e}"))?;
    if !status.is_success() {
        // Upstream error bodies are safe to relay (no key material) and are
        // the only diagnostic the operator gets.
        return Err(format!("upstream mint failed ({status}): {text}"));
    }
    let json: Value = serde_json::from_str(&text)
        .map_err(|e| format!("upstream mint response was not JSON: {e}"))?;
    normalize_upstream(&json, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(role: &str, text: &str) -> (String, String) {
        (role.to_string(), text.to_string())
    }

    #[test]
    fn instructions_without_session_are_header_only() {
        let out = build_instructions(&[]);
        assert!(out.contains("render_component"));
        assert!(out.contains("write_handoff"));
        assert!(!out.contains("Current chat session"));
    }

    #[test]
    fn instructions_keep_transcript_tail_in_order() {
        let transcript: Vec<_> = (0..40)
            .map(|i| entry("user", &format!("message {i}")))
            .collect();
        let out = build_instructions(&transcript);
        // Only the newest BRIEFING_MAX_ENTRIES survive…
        assert!(
            !out.contains("message 9\n"),
            "oldest entries must be dropped"
        );
        assert!(out.contains("message 10"));
        assert!(out.contains("message 39"));
        // …and order is oldest-first among the survivors.
        let a = out.find("message 10").unwrap();
        let b = out.find("message 39").unwrap();
        assert!(a < b, "surviving tail must read oldest-first");
    }

    #[test]
    fn instructions_respect_char_budget_keeping_newest() {
        let big = "x".repeat(3_000);
        let transcript = vec![
            entry("user", &format!("OLDEST {big}")),
            entry("assistant", &format!("MID-A {big}")),
            entry("user", &format!("MID-B {big}")),
            entry("assistant", "NEWEST short"),
        ];
        let out = build_instructions(&transcript);
        assert!(out.contains("NEWEST short"));
        assert!(
            !out.contains("OLDEST"),
            "budget overflow must drop the oldest entries, never the newest"
        );
        assert!(out.len() < BRIEFING_CHAR_BUDGET + 1_000);
    }

    #[test]
    fn instructions_skip_empty_texts() {
        let transcript = vec![entry("tool", "   "), entry("user", "real")];
        let out = build_instructions(&transcript);
        assert!(out.contains("user: real"));
        assert!(!out.contains("tool:"));
    }

    #[test]
    fn default_realtime_model_is_public_id_in_upstream_mint_body() {
        let body = upstream_body(DEFAULT_REALTIME_MODEL, "hello");

        assert_eq!(DEFAULT_REALTIME_MODEL, "gpt-realtime-2");
        assert_eq!(body["session"]["model"], "gpt-realtime-2");
    }

    #[test]
    fn upstream_body_carries_model_tools_and_ttl() {
        let body = upstream_body(DEFAULT_REALTIME_MODEL, "hello");
        assert_eq!(body["session"]["model"], DEFAULT_REALTIME_MODEL);
        assert_eq!(body["session"]["instructions"], "hello");
        assert_eq!(body["expires_after"]["seconds"], 600);
        let tools = body["session"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "render_component");
        assert_eq!(tools[1]["name"], "write_handoff");
    }

    #[test]
    fn normalize_accepts_ga_and_nested_shapes() {
        let ga = json!({ "value": "ek_abc", "expires_at": 1234 });
        let out = normalize_upstream(&ga, "m").unwrap();
        assert_eq!(out["client_secret"], "ek_abc");
        assert_eq!(out["expires_at"], 1234);
        assert_eq!(out["model"], "m");

        let nested = json!({ "client_secret": { "value": "ek_n", "expires_at": 9 } });
        let out = normalize_upstream(&nested, "m").unwrap();
        assert_eq!(out["client_secret"], "ek_n");
        assert_eq!(out["expires_at"], 9);
    }

    #[test]
    fn normalize_rejects_secretless_response() {
        assert!(normalize_upstream(&json!({ "ok": true }), "m").is_err());
        assert!(normalize_upstream(&json!({ "value": "" }), "m").is_err());
    }
}
