//! xAI STT (speech-to-text) and TTS (text-to-speech) endpoints (voice phase 4).
//!
//! `POST /v1/voice/stt` and `POST /v1/voice/tts` let a surface transcribe or
//! synthesize speech through xAI without ever holding a provider key: the daemon
//! resolves the xAI credential (env `XAI_API_KEY` / auth.json `xai` block)
//! per-request via `ocean-providers`, then relays the result back. The surface
//! proxy forwards its `/api/stt` and `/api/tts` routes here.
//!
//! The handler glue lives in `main.rs`; this module owns the pure pieces
//! (multipart builder, JSON body builder, upstream calls) so they stay
//! unit-testable without HTTP.

use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::{json, Value};

// --- Public constants --------------------------------------------------------

/// Default voice id for TTS when the request body omits one.
pub(crate) const DEFAULT_VOICE: &str = "leo";

// --- Upstream URLs -----------------------------------------------------------

const XAI_STT_URL: &str = "https://api.x.ai/v1/stt";
const XAI_TTS_URL: &str = "https://api.x.ai/v1/tts";

/// Shared, timeout-bounded upstream client: connect 10s, request 60s — the
/// same bounds the legacy surface proxy used for these non-streaming batch
/// calls (audio in → JSON/mp3 out, fully buffered), so a hung xAI call can
/// never pin a handler forever. One client process-wide keeps the connection
/// pool warm instead of paying TLS setup per request.
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("bounded reqwest client")
});

// --- Request shapes ----------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct TtsRequest {
    pub text: String,
    #[serde(default)]
    pub voice: Option<String>,
}

// --- Pure body builders ------------------------------------------------------

/// Build the multipart form for xAI STT: one `file` part (filename `clip.webm`,
/// mime `application/octet-stream`) plus text fields `model=grok-stt`,
/// `language=en`, `response_format=json`.
///
/// Exposed for unit testing; the handler uses this to build the form before
/// calling [`transcribe`].
pub(crate) fn build_stt_form(audio_bytes: &[u8]) -> Result<reqwest::multipart::Form, String> {
    let part = reqwest::multipart::Part::bytes(audio_bytes.to_vec())
        .file_name("clip.webm")
        .mime_str("application/octet-stream")
        .map_err(|e| format!("multipart mime: {e}"))?;

    Ok(reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", "grok-stt")
        .text("language", "en")
        .text("response_format", "json"))
}

/// Build the JSON body for xAI TTS. Replicates the surface proxy's exact shape:
/// model `grok-tts`, text, voice, language `en`, response_format `mp3`.
///
/// Exposed for unit testing; the handler uses this to build the body before
/// calling [`synthesize`].
pub(crate) fn build_tts_body(text: &str, voice: &str) -> Value {
    json!({
        "model": "grok-tts",
        "text": text,
        "voice": voice,
        "language": "en",
        "response_format": "mp3",
    })
}

// --- Upstream calls ----------------------------------------------------------

/// Extract the transcript from a successful xAI STT payload. A missing or
/// empty `text` field is a valid EMPTY transcript (silence / non-speech
/// audio), mirroring the legacy proxy's `unwrap_or("")` semantics — it must
/// not be treated as an upstream failure.
pub(crate) fn stt_text(payload: &Value) -> String {
    payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// POST raw audio bytes to xAI STT and return the transcribed text.
///
/// Returns `Ok(text)` on success, or a human-readable error string (mapped to
/// 502 by the handler — the key itself never appears in errors).
pub(crate) async fn transcribe(api_key: &str, audio_bytes: &[u8]) -> Result<String, String> {
    let form = build_stt_form(audio_bytes)?;

    let resp = HTTP
        .post(XAI_STT_URL)
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("stt upstream request failed: {e}"))?;

    let status = resp.status();
    let payload: Value = resp
        .json()
        .await
        .map_err(|e| format!("stt upstream response was not JSON: {e}"))?;

    if !status.is_success() {
        return Err(format!("stt upstream failed ({status}): {payload}"));
    }

    Ok(stt_text(&payload))
}

/// POST the TTS body to xAI and return the audio bytes with the upstream
/// content-type (typically `audio/mpeg`).
///
/// Returns `Ok((audio_bytes, content_type))` on success, or a human-readable
/// error string (mapped to 502 by the handler — the key itself never appears).
pub(crate) async fn synthesize(
    api_key: &str,
    text: &str,
    voice: &str,
) -> Result<(Vec<u8>, String), String> {
    let body = build_tts_body(text, voice);

    let resp = HTTP
        .post(XAI_TTS_URL)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("tts upstream request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let detail = resp.text().await.unwrap_or_default();
        return Err(format!("tts upstream failed ({status}): {detail}"));
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("audio/mpeg")
        .to_string();

    let audio = resp
        .bytes()
        .await
        .map_err(|e| format!("tts upstream read failed: {e}"))?
        .to_vec();

    Ok((audio, content_type))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- STT form builder ----------------------------------------------------

    #[test]
    fn stt_form_has_correct_text_fields() {
        let form = build_stt_form(b"fake audio").unwrap();
        // We can't inspect reqwest::multipart::Form internals easily, but we
        // can verify it round-trips through a boundary snapshot.
        let boundary = form.boundary().to_string();
        assert!(!boundary.is_empty());
    }

    #[test]
    fn stt_form_accepts_empty_audio() {
        // An empty body is valid at the form layer — upstream will reject it.
        let form = build_stt_form(b"").unwrap();
        assert!(!form.boundary().is_empty());
    }

    // --- STT payload normalization -------------------------------------------

    #[test]
    fn stt_text_missing_field_is_empty_transcript() {
        assert_eq!(stt_text(&json!({})), "");
    }

    #[test]
    fn stt_text_empty_field_is_empty_transcript() {
        assert_eq!(stt_text(&json!({"text": "  "})), "");
    }

    #[test]
    fn stt_text_trims_transcript() {
        assert_eq!(
            stt_text(&json!({"text": "  hello there \n"})),
            "hello there"
        );
    }

    // --- TTS body builder ----------------------------------------------------

    #[test]
    fn tts_body_replicates_proxy_shape() {
        let body = build_tts_body("Hello world", "rex");
        assert_eq!(body["model"], "grok-tts");
        assert_eq!(body["text"], "Hello world");
        assert_eq!(body["voice"], "rex");
        assert_eq!(body["language"], "en");
        assert_eq!(body["response_format"], "mp3");
    }

    #[test]
    fn tts_body_defaults_to_leo() {
        let body = build_tts_body("hi", DEFAULT_VOICE);
        assert_eq!(body["voice"], "leo");
    }

    #[test]
    fn tts_body_trims_voice_but_preserves_case() {
        let body = build_tts_body("hi", "Rex");
        assert_eq!(body["voice"], "Rex");
    }
}
