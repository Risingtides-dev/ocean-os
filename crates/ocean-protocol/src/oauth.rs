//! OAuth refresh-token exchange for provider subscription logins.
//!
//! Pure HTTP: POST a `refresh_token` grant to a token endpoint and return the
//! new tokens. Which blocks to refresh, and persisting them back into Ocean's
//! `auth.json`, is the caller's job (`ocean-agent`) — this module knows nothing
//! about files or providers, so it is trivially testable against a local fake
//! endpoint.

use std::time::Duration;

use serde::Deserialize;

use crate::error::{Error, Result};

/// Deadline for one token-endpoint round trip. A refresh runs on the turn's
/// critical path (before credential resolution), so it must fail fast rather
/// than hold the turn hostage.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

/// A successful refresh: the new access token, an optional rotated refresh
/// token (some issuers rotate on every use, some don't), and the lifetime.
#[derive(Debug, Clone)]
pub struct RefreshedToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in_secs: Option<i64>,
}

#[derive(Deserialize)]
struct WireResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// Exchange a refresh token at `endpoint` for fresh credentials.
///
/// Sends the JSON `refresh_token` grant shape both Anthropic's and OpenAI's
/// public-client token endpoints accept. The refresh token itself is a
/// secret — it is never logged, and errors carry only the HTTP status plus the
/// endpoint, never the body echoed verbatim (bodies can quote the token).
pub async fn refresh_token(
    endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<RefreshedToken> {
    let client = reqwest::Client::builder()
        .timeout(REFRESH_TIMEOUT)
        .build()
        .map_err(Error::Http)?;
    let resp = client
        .post(endpoint)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": client_id,
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .map_err(Error::Http)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::ProviderError {
            status: status.as_u16(),
            body: format!("token refresh at {endpoint} failed"),
        });
    }
    let wire: WireResponse = resp
        .json()
        .await
        .map_err(|e| Error::InvalidResponse(format!("token refresh response: {e}")))?;
    if wire.access_token.trim().is_empty() {
        return Err(Error::InvalidResponse(
            "token refresh returned an empty access_token".into(),
        ));
    }
    Ok(RefreshedToken {
        access_token: wire.access_token,
        refresh_token: wire.refresh_token,
        expires_in_secs: wire.expires_in,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A one-shot fake token endpoint returning `body` with `status`.
    async fn fake_endpoint(status: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Drain the request head (we don't parse it; the test asserts on
            // the response handling only).
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        format!("http://{addr}/oauth/token")
    }

    #[tokio::test]
    async fn successful_refresh_parses_tokens() {
        let url = fake_endpoint(
            "200 OK",
            r#"{"access_token":"new-at","refresh_token":"new-rt","expires_in":3600}"#,
        )
        .await;
        let out = refresh_token(&url, "client-1", "old-rt").await.unwrap();
        assert_eq!(out.access_token, "new-at");
        assert_eq!(out.refresh_token.as_deref(), Some("new-rt"));
        assert_eq!(out.expires_in_secs, Some(3600));
    }

    #[tokio::test]
    async fn http_error_maps_to_provider_error_without_leaking_body() {
        let url = fake_endpoint("400 Bad Request", r#"{"error":"invalid_grant"}"#).await;
        let err = refresh_token(&url, "client-1", "old-rt")
            .await
            .expect_err("400 must be an error");
        let msg = format!("{err}");
        assert!(msg.contains("400"), "{msg}");
        assert!(
            !msg.contains("invalid_grant"),
            "response body must not be echoed into the error (may quote secrets): {msg}"
        );
    }

    #[tokio::test]
    async fn empty_access_token_is_rejected() {
        let url = fake_endpoint("200 OK", r#"{"access_token":""}"#).await;
        assert!(refresh_token(&url, "c", "r").await.is_err());
    }
}
