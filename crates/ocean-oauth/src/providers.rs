//! Provider constants, authorize-URL builders, and token exchange.
//!
//! Mirrors OMP's `registry/oauth/{anthropic,openai-codex}.ts`. Anthropic posts
//! a JSON body; Codex posts a form body and extracts `accountId` from the
//! unsigned access-token JWT.

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;

use crate::server::BindSpec;
use crate::util::{build_query, now_millis};
use crate::OAuthProvider;

/// Codex `originator` constant — same value `ocean-protocol`'s codex provider
/// already sends.
const CODEX_ORIGINATOR: &str = "codex_cli_rs";
/// JWT claim path carrying the ChatGPT account id.
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
/// Five-minute safety margin applied to Anthropic expiry (per OMP).
const ANTHROPIC_EXPIRY_MARGIN_MS: i64 = 300_000;

pub(crate) struct ProviderConsts {
    pub client_id: &'static str,
    pub authorize_url: &'static str,
    pub token_url: &'static str,
    pub scope: &'static str,
}

pub(crate) const ANTHROPIC: ProviderConsts = ProviderConsts {
    client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
    authorize_url: "https://claude.ai/oauth/authorize",
    token_url: "https://api.anthropic.com/v1/oauth/token",
    scope: "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
};

pub(crate) const CODEX: ProviderConsts = ProviderConsts {
    client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    authorize_url: "https://auth.openai.com/oauth/authorize",
    token_url: "https://auth.openai.com/oauth/token",
    scope: "openid profile email offline_access api.connectors.read api.connectors.invoke",
};

pub(crate) fn consts(provider: OAuthProvider) -> &'static ProviderConsts {
    match provider {
        OAuthProvider::Claude => &ANTHROPIC,
        OAuthProvider::Codex => &CODEX,
    }
}

pub(crate) fn bind_spec(provider: OAuthProvider) -> BindSpec {
    match provider {
        OAuthProvider::Claude => BindSpec {
            callback_path: "/callback",
            preferred_port: 54545,
            allow_fallback: true,
            fixed_redirect_uri: None,
            label: "claude",
        },
        OAuthProvider::Codex => BindSpec {
            callback_path: "/auth/callback",
            preferred_port: 1455,
            allow_fallback: false,
            fixed_redirect_uri: Some("http://localhost:1455/auth/callback"),
            label: "codex",
        },
    }
}

/// Build the provider authorize URL with PKCE + state. Param order mirrors OMP.
pub(crate) fn build_authorize_url(
    provider: OAuthProvider,
    state: &str,
    challenge: &str,
    redirect_uri: &str,
) -> String {
    let c = consts(provider);
    match provider {
        OAuthProvider::Claude => {
            let params: [(&str, &str); 8] = [
                ("code", "true"),
                ("client_id", c.client_id),
                ("response_type", "code"),
                ("redirect_uri", redirect_uri),
                ("scope", c.scope),
                ("code_challenge", challenge),
                ("code_challenge_method", "S256"),
                ("state", state),
            ];
            format!("{}?{}", c.authorize_url, build_query(&params))
        }
        OAuthProvider::Codex => {
            let params: [(&str, &str); 10] = [
                ("response_type", "code"),
                ("client_id", c.client_id),
                ("redirect_uri", redirect_uri),
                ("scope", c.scope),
                ("code_challenge", challenge),
                ("code_challenge_method", "S256"),
                ("state", state),
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("originator", CODEX_ORIGINATOR),
            ];
            format!("{}?{}", c.authorize_url, build_query(&params))
        }
    }
}

pub(crate) struct ExchangedToken {
    pub access: String,
    pub refresh: String,
    pub expires_ms: i64,
    pub account_id: Option<String>,
}

/// Exchange an authorization code for tokens at the given (possibly overridden)
/// token endpoint.
pub(crate) async fn exchange(
    provider: OAuthProvider,
    token_url: &str,
    code: &str,
    state: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<ExchangedToken> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    match provider {
        OAuthProvider::Claude => {
            exchange_anthropic(&client, token_url, code, state, redirect_uri, verifier).await
        }
        OAuthProvider::Codex => exchange_codex(&client, token_url, code, redirect_uri, verifier).await,
    }
}

async fn exchange_anthropic(
    client: &reqwest::Client,
    token_url: &str,
    code: &str,
    state: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<ExchangedToken> {
    // Defensive: a manually pasted code may embed `code#state`.
    let (code, state) = split_code_fragment(code, state);

    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": ANTHROPIC.client_id,
        "code": code,
        "state": state,
        "redirect_uri": redirect_uri,
        "code_verifier": verifier,
    });

    // JSON body, Content-Type set explicitly, deliberately NO Accept header.
    let resp = client
        .post(token_url)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .context("anthropic token exchange request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("anthropic token exchange failed: {status} {text}");
    }

    let parsed: AnthropicTokenResponse = resp
        .json()
        .await
        .context("anthropic token response was not valid JSON")?;

    let expires_ms =
        now_millis() + parsed.expires_in.saturating_mul(1000) - ANTHROPIC_EXPIRY_MARGIN_MS;
    let account_id = parsed.account.map(|a| a.uuid);

    Ok(ExchangedToken {
        access: parsed.access_token,
        refresh: parsed.refresh_token,
        expires_ms,
        account_id,
    })
}

async fn exchange_codex(
    client: &reqwest::Client,
    token_url: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<ExchangedToken> {
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", CODEX.client_id),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
    ];
    // `.form()` sets Content-Type: application/x-www-form-urlencoded.
    let resp = client
        .post(token_url)
        .form(&form)
        .send()
        .await
        .context("codex token exchange request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("codex token exchange failed: {status} {text}");
    }

    let parsed: CodexTokenResponse = resp
        .json()
        .await
        .context("codex token response was not valid JSON")?;

    let account_id = extract_account_id(&parsed.access_token)?;
    let expires_ms = now_millis() + parsed.expires_in.saturating_mul(1000);

    Ok(ExchangedToken {
        access: parsed.access_token,
        refresh: parsed.refresh_token,
        expires_ms,
        account_id: Some(account_id),
    })
}

#[derive(Deserialize)]
struct AnthropicTokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    #[serde(default)]
    account: Option<AnthropicAccount>,
}

#[derive(Deserialize)]
struct AnthropicAccount {
    uuid: String,
    #[serde(default)]
    #[allow(dead_code)]
    email_address: Option<String>,
}

#[derive(Deserialize)]
struct CodexTokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

/// Split a manual-paste `code#state` artifact; keeps the original state when no
/// fragment is present or the fragment is empty.
fn split_code_fragment(code: &str, state: &str) -> (String, String) {
    if let Some(idx) = code.find('#') {
        let new_code = code[..idx].to_string();
        let fragment = &code[idx + 1..];
        let new_state = if fragment.is_empty() {
            state.to_string()
        } else {
            fragment.to_string()
        };
        (new_code, new_state)
    } else {
        (code.to_string(), state.to_string())
    }
}

/// Decode the unsigned Codex access token and read the ChatGPT account id.
fn extract_account_id(access_token: &str) -> Result<String> {
    let payload = decode_jwt_payload(access_token);
    let account_id = payload
        .as_ref()
        .and_then(|v| v.get(JWT_CLAIM_PATH))
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty());
    match account_id {
        Some(id) => Ok(id),
        None => Err(anyhow!("failed to extract accountId from token")),
    }
}

/// Best-effort decode of an unsigned JWT payload (base64url, padding-tolerant).
fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let stripped = parts[1].trim_end_matches('=');
    let bytes = URL_SAFE_NO_PAD.decode(stripped).ok()?;
    serde_json::from_slice::<serde_json::Value>(&bytes).ok()
}

/// Build the Ocean auth-file block for a provider's exchanged token.
pub(crate) fn build_block(provider: OAuthProvider, token: &ExchangedToken) -> serde_json::Value {
    match provider {
        OAuthProvider::Claude => {
            if let Some(id) = &token.account_id {
                serde_json::json!({
                    "type": "oauth",
                    "access": token.access,
                    "refresh": token.refresh,
                    "expires": token.expires_ms,
                    "accountId": id,
                })
            } else {
                serde_json::json!({
                    "type": "oauth",
                    "access": token.access,
                    "refresh": token.refresh,
                    "expires": token.expires_ms,
                })
            }
        }
        OAuthProvider::Codex => {
            // Codex always carries an accountId (required at exchange time).
            let id = token
                .account_id
                .clone()
                .expect("codex tokens always carry an accountId");
            serde_json::json!({
                "type": "oauth",
                "access": token.access,
                "refresh": token.refresh,
                "expires": token.expires_ms,
                "accountId": id,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_authorize_url, build_block, consts, decode_jwt_payload, extract_account_id,
        ExchangedToken, CODEX,
    };
    use crate::util::query_get;
    use crate::OAuthProvider;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use serde_json::{json, Value};

    fn query_of(url: &str) -> &str {
        url.split_once('?').map(|(_, q)| q).unwrap_or("")
    }

    // ---- #2 authorize URLs -------------------------------------------------

    #[test]
    fn claude_authorize_url_has_all_required_params() {
        let url = build_authorize_url(
            OAuthProvider::Claude,
            "STATER",
            "CHA1",
            "http://localhost:54545/callback",
        );
        assert!(url.starts_with("https://claude.ai/oauth/authorize?"), "{url}");
        let q = query_of(&url);
        assert_eq!(query_get(q, "code"), Some("true".to_string()));
        assert_eq!(
            query_get(q, "client_id"),
            Some("9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_string())
        );
        assert_eq!(query_get(q, "response_type"), Some("code".to_string()));
        assert_eq!(
            query_get(q, "redirect_uri"),
            Some("http://localhost:54545/callback".to_string())
        );
        assert_eq!(query_get(q, "code_challenge"), Some("CHA1".to_string()));
        assert_eq!(query_get(q, "code_challenge_method"), Some("S256".to_string()));
        assert_eq!(query_get(q, "state"), Some("STATER".to_string()));

        // The full 6-scope string, decoded back to the exact constant.
        let scope = query_get(q, "scope").expect("scope present");
        assert_eq!(scope, consts(OAuthProvider::Claude).scope);
        assert_eq!(scope.split_whitespace().count(), 6, "expected 6 scopes: {scope}");

        // Scopes are percent-encoded in the raw query (space -> %20, colon -> %3A).
        assert!(q.contains("user%3Asessions%3Aclaude_code"), "raw q: {q}");
        assert!(q.contains("org%3Acreate_api_key%20"), "raw q: {q}");
    }

    #[test]
    fn codex_authorize_url_has_all_required_params() {
        let url = build_authorize_url(
            OAuthProvider::Codex,
            "ST",
            "CHA2",
            "http://localhost:1455/auth/callback",
        );
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"), "{url}");
        let q = query_of(&url);
        assert_eq!(query_get(q, "response_type"), Some("code".to_string()));
        assert_eq!(
            query_get(q, "client_id"),
            Some("app_EMoamEEZ73f0CkXaXp7hrann".to_string())
        );
        assert_eq!(
            query_get(q, "redirect_uri"),
            Some("http://localhost:1455/auth/callback".to_string())
        );
        assert_eq!(query_get(q, "code_challenge"), Some("CHA2".to_string()));
        assert_eq!(query_get(q, "code_challenge_method"), Some("S256".to_string()));
        assert_eq!(query_get(q, "state"), Some("ST".to_string()));
        assert_eq!(query_get(q, "id_token_add_organizations"), Some("true".to_string()));
        assert_eq!(query_get(q, "codex_cli_simplified_flow"), Some("true".to_string()));
        assert_eq!(query_get(q, "originator"), Some("codex_cli_rs".to_string()));

        let scope = query_get(q, "scope").expect("scope present");
        assert_eq!(scope, CODEX.scope);
        assert!(scope.contains("offline_access"));
    }

    #[test]
    fn authorize_url_param_order_is_stable() {
        // Order mirrors OMP; verify by raw key positions.
        let url = build_authorize_url(
            OAuthProvider::Claude,
            "S",
            "C",
            "http://localhost:1/callback",
        );
        let q = query_of(&url);
        let pos = |key: &str| q.find(&format!("{key}=")).unwrap_or_else(|| panic!("missing {key} in {q}"));
        assert!(pos("code") < pos("client_id"));
        assert!(pos("client_id") < pos("response_type"));
        assert!(pos("response_type") < pos("redirect_uri"));
        assert!(pos("redirect_uri") < pos("scope"));
        assert!(pos("scope") < pos("code_challenge"));
        assert!(pos("code_challenge") < pos("code_challenge_method"));
        assert!(pos("code_challenge_method") < pos("state"));
    }

    // ---- #5 JWT accountId extraction --------------------------------------

    fn jwt(payload: &Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
        let sig = URL_SAFE_NO_PAD.encode(b"sig");
        format!("{header}.{body}.{sig}")
    }

    #[test]
    fn extract_account_id_reads_chatgpt_claim() {
        let token = jwt(&json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_abc-123" }
        }));
        assert_eq!(extract_account_id(&token).unwrap(), "acct_abc-123");
    }

    #[test]
    fn decode_jwt_payload_strips_trailing_base64_padding() {
        // Some issuers emit standard base64 (with '=' padding). The decoder
        // trims it before URL_SAFE_NO_PAD decoding — verify that path.
        let payload =
            serde_json::to_vec(&json!({"sub":"u","https://api.openai.com/auth":{"chatgpt_account_id":"a"}}))
                .unwrap();
        let unpadded = URL_SAFE_NO_PAD.encode(&payload);
        // Reconstruct the correct padded form for this payload length.
        let padded = match unpadded.len() % 4 {
            2 => format!("{unpadded}=="),
            3 => format!("{unpadded}="),
            _ => unpadded.clone(),
        };
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let token = format!("{header}.{padded}.");
        let v = decode_jwt_payload(&token).expect("decodes despite padding");
        assert_eq!(v["https://api.openai.com/auth"]["chatgpt_account_id"], "a");
    }

    #[test]
    fn extract_account_id_missing_claim_is_error() {
        let token = jwt(&json!({ "sub": "u1" }));
        let err = extract_account_id(&token).unwrap_err();
        assert!(
            err.to_string()
                .contains("failed to extract accountId from token"),
            "err: {err}"
        );
    }

    #[test]
    fn extract_account_id_empty_value_is_error() {
        let token = jwt(&json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "" }
        }));
        assert!(extract_account_id(&token).is_err());
    }

    #[test]
    fn extract_account_id_malformed_token_is_error() {
        assert!(extract_account_id("only.two").is_err()); // not 3 segments
        assert!(extract_account_id("not-a-jwt").is_err());
    }

    // ---- #7 build_block ----------------------------------------------------

    fn token(access: &str, expires: i64, account: Option<&str>) -> ExchangedToken {
        ExchangedToken {
            access: access.to_string(),
            refresh: "rt".to_string(),
            expires_ms: expires,
            account_id: account.map(str::to_string),
        }
    }

    #[test]
    fn codex_block_always_has_account_id() {
        let blk = build_block(OAuthProvider::Codex, &token("a", 1_700_000_000_000i64, Some("acct_x")));
        assert_eq!(blk["type"], "oauth");
        assert_eq!(blk["access"], "a");
        assert_eq!(blk["refresh"], "rt");
        assert_eq!(blk["expires"], json!(1_700_000_000_000i64));
        assert_eq!(blk["accountId"], "acct_x");
    }

    #[test]
    fn claude_block_omits_account_id_when_absent() {
        let blk = build_block(OAuthProvider::Claude, &token("a", 42, None));
        assert_eq!(blk["type"], "oauth");
        assert_eq!(blk["access"], "a");
        assert_eq!(blk["refresh"], "rt");
        assert_eq!(blk["expires"], json!(42));
        assert!(
            blk.get("accountId").is_none(),
            "claude block must omit accountId when absent: {blk}"
        );
    }

    #[test]
    fn claude_block_includes_account_id_when_present() {
        let blk = build_block(OAuthProvider::Claude, &token("a", 42, Some("acct_y")));
        assert_eq!(blk["accountId"], "acct_y");
        assert_eq!(blk["expires"], json!(42));
    }

    #[test]
    fn build_block_expires_passes_through_verbatim() {
        for expires in [0i64, -1, 1_700_000_000_000, i64::MAX] {
            let blk = build_block(OAuthProvider::Claude, &token("a", expires, None));
            assert_eq!(blk["expires"], json!(expires), "expires not passed through");
        }
    }
}
