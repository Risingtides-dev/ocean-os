//! Error humanizer — turns daemon/reqwest error strings into terse, readable
//! messages for the TUI transcript and status line. Idempotent for strings that
//! are already human-readable (no "humanize(humanize(x))" corruption).

/// Check whether `s` matches a known transport / DNS / timeout pattern.
fn is_connect_pattern(s: &str) -> bool {
    let lower = s.to_lowercase();
    lower.contains("error sending request for url")
        || lower.contains("connection refused")
        || lower.contains("dns error")
        || lower.contains("failed to connect")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("deadline has elapsed")
}

/// Classify the raw error string (before prefix stripping) as connection-shaped.
/// Mirrors the combined checks inside `humanize` so that callers can pick the
/// right transcript prefix for `TurnSendFailed` without duplicating the logic.
pub(crate) fn is_connect_shaped(err: &str) -> bool {
    if is_connect_pattern(err) {
        return true;
    }
    // Also check the body after stripping the "turn: "/"session: " prefix
    // that app.rs prepends.
    match err
        .strip_prefix("turn: ")
        .or_else(|| err.strip_prefix("session: "))
    {
        Some(stripped) => is_connect_pattern(stripped),
        None => false,
    }
}

/// Convert a raw error string into a human-readable message suitable for the
/// transcript or status line. Already-human strings pass through unchanged.
pub fn humanize(err: &str) -> String {
    if is_connect_pattern(err) {
        // Distinguish timeout from other transport failures.
        let lower = err.to_lowercase();
        if lower.contains("timed out")
            || lower.contains("timeout")
            || lower.contains("deadline has elapsed")
        {
            return "daemon timed out — is it overloaded?".into();
        }
        if let Some(host) = extract_url_host(err) {
            return format!("can't reach the daemon at {host}");
        }
        return "can't reach the daemon — is it running?".into();
    }

    // ── turn: / session: prefix from app.rs:2610,2638 ────────────────────
    let body = err
        .strip_prefix("turn: ")
        .or_else(|| err.strip_prefix("session: "))
        .unwrap_or(err);

    // ── JSON provider body: extract .error.message or .message ───────────
    // Then classify the extracted message for credentials too, so a JSON
    // body like {"error":{"message":"token_invalidated"}} still gets the
    // /login guidance instead of a bare "token_invalidated".
    if let Some(msg) = extract_json_error(body) {
        if let Some(cred) = classify_credential(&msg) {
            return cred;
        }
        return msg;
    }

    // ── credential-shaped on the stripped body ───────────────────────────
    if let Some(cred) = classify_credential(body) {
        return cred;
    }

    // ── connection-shaped on the stripped body ───────────────────────────
    if is_connect_pattern(body) {
        if let Some(host) = extract_url_host(body) {
            return format!("can't reach the daemon at {host}");
        }
        return "can't reach the daemon — is it running?".into();
    }

    // ── fallback: first 120 chars ────────────────────────────────────────
    if body.len() <= 120 {
        body.to_string()
    } else {
        body.chars().take(117).collect::<String>() + "..."
    }
}

/// Check whether `s` matches a known credential-related pattern.
/// Returns a human-readable recovery message if it does, `None` otherwise.
fn classify_credential(s: &str) -> Option<String> {
    let lower = s.to_lowercase();
    if lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("token_invalidated")
        || lower.contains("invalid_grant")
        || lower.contains("expired")
        || lower.contains("oauth")
        || lower.contains("authentication")
    {
        return Some("credentials look expired or revoked — run /login to reconnect".into());
    }
    if lower.contains("no credential")
        || lower.contains("not configured")
        || lower.contains("missing api key")
        || lower.contains("no api key")
    {
        return Some(
            "no credentials for this model — /login to add one, or /model to switch".into(),
        );
    }
    None
}

/// Try to extract an `error.message` or `message` field from a JSON body.
fn extract_json_error(body: &str) -> Option<String> {
    // Find a JSON object-looking region.
    let start = body.find('{')?;
    let slice = &body[start..];
    let v: serde_json::Value = serde_json::from_str(slice).ok()?;
    if let Some(msg) = v
        .pointer("/error/message")
        .or_else(|| v.pointer("/message"))
        .and_then(serde_json::Value::as_str)
    {
        let msg = msg.trim();
        if !msg.is_empty() {
            return Some(msg.to_string());
        }
    }
    None
}

/// Pull a `host:port` out of a URL string like `(http://127.0.0.1:4780/...)`.
fn extract_url_host(err: &str) -> Option<String> {
    // Find "http://" or "https://" inside the error.
    let scheme_pos = err.find("http://").or_else(|| err.find("https://"))?;
    let after_scheme = &err[scheme_pos..];
    // Scan until whitespace, ')', or end.
    let end = after_scheme
        .find(|c: char| c.is_whitespace() || c == ')' || c == ',')
        .unwrap_or(after_scheme.len());
    let url_str = &after_scheme[..end];
    // Strip the scheme prefix.
    let rest = url_str
        .strip_prefix("http://")
        .or_else(|| url_str.strip_prefix("https://"))?;
    // Take only the host:port part, drop path.
    let host_end = rest.find('/').unwrap_or(rest.len());
    Some(rest[..host_end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reqwest_url_error() {
        let got = humanize("error sending request for url (http://127.0.0.1:4780/v1/agent/turns): connection closed before message completed");
        assert_eq!(got, "can't reach the daemon at 127.0.0.1:4780");
    }

    #[test]
    fn connection_refused() {
        let got = humanize("tcp connect error: Connection refused (os error 61)");
        assert_eq!(got, "can't reach the daemon — is it running?");
    }

    #[test]
    fn timeout() {
        let got = humanize("operation timed out");
        assert_eq!(got, "daemon timed out — is it overloaded?");
    }

    #[test]
    fn deadline_elapsed() {
        let got = humanize("deadline has elapsed");
        assert_eq!(got, "daemon timed out — is it overloaded?");
    }

    #[test]
    fn dns_error() {
        let got = humanize("dns error: failed to resolve");
        assert_eq!(got, "can't reach the daemon — is it running?");
    }

    #[test]
    fn json_provider_error_message() {
        let got = humanize(
            "turn: HTTP 500: {\"error\":{\"message\":\"rate limit exceeded\",\"code\":429}}",
        );
        assert_eq!(got, "rate limit exceeded");
    }

    #[test]
    fn json_provider_message_only() {
        let got = humanize("turn: {\"message\":\"model overloaded\"}");
        assert_eq!(got, "model overloaded");
    }

    #[test]
    fn json_no_error_field() {
        let got = humanize("turn: {\"status\":\"ok\"}");
        assert_eq!(got, "{\"status\":\"ok\"}");
    }

    #[test]
    fn credential_401() {
        let got = humanize("turn: HTTP 401 Unauthorized");
        assert_eq!(
            got,
            "credentials look expired or revoked — run /login to reconnect"
        );
    }

    #[test]
    fn credential_token_invalidated() {
        let got = humanize("turn: token_invalidated");
        assert_eq!(
            got,
            "credentials look expired or revoked — run /login to reconnect"
        );
    }

    #[test]
    fn credential_invalid_grant() {
        let got = humanize("turn: invalid_grant");
        assert_eq!(
            got,
            "credentials look expired or revoked — run /login to reconnect"
        );
    }

    #[test]
    fn credential_oauth_expired() {
        let got = humanize("turn: oauth token expired");
        assert_eq!(
            got,
            "credentials look expired or revoked — run /login to reconnect"
        );
    }

    #[test]
    fn credential_authentication() {
        let got = humanize("turn: authentication failed");
        assert_eq!(
            got,
            "credentials look expired or revoked — run /login to reconnect"
        );
    }

    #[test]
    fn no_credential() {
        let got = humanize("turn: no credential found for provider deepseek");
        assert_eq!(
            got,
            "no credentials for this model — /login to add one, or /model to switch"
        );
    }

    #[test]
    fn not_configured() {
        let got = humanize("turn: provider not configured: glm");
        assert_eq!(
            got,
            "no credentials for this model — /login to add one, or /model to switch"
        );
    }

    #[test]
    fn missing_api_key() {
        let got = humanize("turn: missing api key for openai");
        assert_eq!(
            got,
            "no credentials for this model — /login to add one, or /model to switch"
        );
    }

    #[test]
    fn json_token_invalidated_gets_login_hint() {
        let got = humanize("turn: {\"error\":{\"message\":\"token_invalidated\"}}");
        assert_eq!(
            got,
            "credentials look expired or revoked — run /login to reconnect"
        );
    }

    #[test]
    fn json_expired_token_gets_login_hint() {
        let got = humanize("turn: HTTP 500: {\"error\":{\"message\":\"oauth token expired\"}}");
        assert_eq!(
            got,
            "credentials look expired or revoked — run /login to reconnect"
        );
    }

    #[test]
    fn json_unauthorized_gets_login_hint() {
        let got = humanize("turn: {\"message\":\"Unauthorized\"}");
        assert_eq!(
            got,
            "credentials look expired or revoked — run /login to reconnect"
        );
    }
    #[test]
    fn turn_prefix_stripped() {
        let got = humanize("turn: HTTP 401 Unauthorized");
        assert_eq!(
            got,
            "credentials look expired or revoked — run /login to reconnect"
        );
    }

    #[test]
    fn session_prefix_stripped() {
        let got = humanize("session: Connection refused (os error 61)");
        assert_eq!(got, "can't reach the daemon — is it running?");
    }

    #[test]
    fn already_human_passes_through() {
        let got = humanize("can't reach the daemon at 127.0.0.1:4780");
        assert_eq!(got, "can't reach the daemon at 127.0.0.1:4780");
    }

    #[test]
    fn short_fallback() {
        let got = humanize("something broke");
        assert_eq!(got, "something broke");
    }

    #[test]
    fn long_fallback_truncated() {
        let long = "a".repeat(200);
        let got = humanize(&long);
        assert!(got.ends_with("..."));
        assert_eq!(got.len(), 120); // 117 chars + "..."
    }

    // ── is_connect_shaped ────────────────────────────────────────────────

    #[test]
    fn is_connect_shaped_connection_refused() {
        assert!(is_connect_shaped(
            "tcp connect error: Connection refused (os error 61)"
        ));
    }

    #[test]
    fn is_connect_shaped_timeout() {
        assert!(is_connect_shaped("operation timed out"));
    }

    #[test]
    fn is_connect_shaped_dns() {
        assert!(is_connect_shaped("dns error: failed to resolve"));
    }

    #[test]
    fn is_connect_shaped_on_stripped_body() {
        assert!(is_connect_shaped(
            "session: Connection refused (os error 61)"
        ));
        assert!(is_connect_shaped("turn: dns error: failed to resolve"));
    }

    #[test]
    fn is_connect_shaped_rejects_credentials() {
        assert!(!is_connect_shaped("turn: HTTP 401 Unauthorized"));
        assert!(!is_connect_shaped("turn: token_invalidated"));
        assert!(!is_connect_shaped(
            "turn: no credential found for provider deepseek"
        ));
    }

    #[test]
    fn is_connect_shaped_rejects_decode_errors() {
        assert!(!is_connect_shaped("turn: failed to decode JSON response"));
        assert!(!is_connect_shaped("turn: HTTP 500: internal error"));
    }
}
