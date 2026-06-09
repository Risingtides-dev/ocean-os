//! LiveKit join-token minting (OCEAN-137).
//!
//! The web surface renders a LiveKit join panel and `POST`s to the daemon's
//! `/v1/rooms/{room_id}/livekit-token` route to mint a room JWT it can hand to
//! the `livekit-client` SDK. The token signing lives here so the daemon route
//! stays a thin handler and the credential/grant logic is unit-tested.
//!
//! Minting only needs the three LiveKit auth vars — NOT the Twilio SIP trunk /
//! caller-number that [`crate::SipConfig`] also requires. So a deployment with a
//! LiveKit Cloud account but no telephony trunk can still join in-room voice/
//! video on the web. We deliberately read those three directly rather than going
//! through `SipConfig::from_env()`, which would 503 on a missing trunk.

use chrono::{SecondsFormat, Utc};
use livekit_api::access_token::{AccessToken, VideoGrants};
use serde::{Deserialize, Serialize};

/// The minimal LiveKit credentials needed to mint a room join token: the host
/// URL the client connects to, plus the server API key/secret used to sign.
#[derive(Debug, Clone)]
pub struct LiveKitTokenConfig {
    /// LiveKit host the client connects to (https/wss URL).
    pub url: String,
    /// LiveKit server API key.
    pub api_key: String,
    /// LiveKit server API secret (signs the JWT). Never logged.
    pub api_secret: String,
}

impl LiveKitTokenConfig {
    /// Source the three LiveKit auth vars from the environment, the way the
    /// daemon does at request time. Returns Err naming the first missing var so
    /// the route can tell the operator exactly what to set — and crucially does
    /// NOT require the SIP trunk / caller number that full telephony needs.
    ///
    /// - `LIVEKIT_URL` — the LiveKit Cloud host (https/wss)
    /// - `LIVEKIT_API_KEY`, `LIVEKIT_API_SECRET`
    pub fn from_env() -> Result<Self, String> {
        let var = |k: &str| std::env::var(k).map_err(|_| format!("{k} not set"));
        let config = LiveKitTokenConfig {
            url: var("LIVEKIT_URL")?,
            api_key: var("LIVEKIT_API_KEY")?,
            api_secret: var("LIVEKIT_API_SECRET")?,
        };
        config.validate()?;
        Ok(config)
    }

    /// Validate the config is complete enough to sign a token.
    pub fn validate(&self) -> Result<(), String> {
        if self.url.trim().is_empty() {
            return Err("LIVEKIT_URL is empty".into());
        }
        if !self.url.starts_with("http") && !self.url.starts_with("wss") {
            return Err("LIVEKIT_URL must be an http(s)/wss URL".into());
        }
        if self.api_key.trim().is_empty() || self.api_secret.trim().is_empty() {
            return Err("LiveKit api_key/api_secret missing".into());
        }
        Ok(())
    }
}

/// What the surface POSTs to `/v1/rooms/{room_id}/livekit-token`. Mirrors the
/// body the web bridge sends (`index.html` `oceanLiveKitConnect`) and the
/// read-only tap request in [`crate::room_tap`]. All fields default so a sparse
/// body (or the room-tap shape) still parses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiveKitTokenRequest {
    #[serde(default)]
    pub surface_id: String,
    #[serde(default)]
    pub participant_id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default = "default_true")]
    pub can_publish: bool,
    #[serde(default = "default_true")]
    pub can_subscribe: bool,
}

fn default_true() -> bool {
    true
}

impl Default for LiveKitTokenRequest {
    fn default() -> Self {
        Self {
            surface_id: String::new(),
            participant_id: String::new(),
            display_name: String::new(),
            can_publish: true,
            can_subscribe: true,
        }
    }
}

/// The response shape the web bridge decodes: it needs `token` + `url` and
/// checks `ok`. `room` echoes the room id back for the SDK status line.
///
/// `expires_at` is the instant `token` stops being valid (`now + TOKEN_TTL`),
/// emitted as an RFC3339/UTC string. The surface declares this field with NO
/// serde default (`ocean-gui` `shell::daemon::LiveKitTokenResponse`) and threads
/// it into live connection state so it can pre-emptively refresh before the TTL
/// cliff (OCEAN-240). Emitting it is a contract requirement, not optional —
/// omit it and the surface either fails to deserialize the payload or
/// zero-values the expiry and drops mid-call at the 6h boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiveKitTokenResponse {
    pub ok: bool,
    pub url: String,
    pub token: String,
    pub room: String,
    /// RFC3339 UTC timestamp (`Z`-suffixed, e.g. `2026-06-03T22:00:00Z`) at
    /// which `token` expires — the exact shape the surface parses.
    pub expires_at: String,
}

/// How long a minted join token stays valid. Six hours comfortably covers a
/// long call/meeting; LiveKit refreshes the session itself once connected.
const TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// Whether a minted token may PUBLISH media into the room — the load-bearing
/// capability, because publishing means injecting audio/video into a live
/// call/meeting.
///
/// OCEAN-220 (P0): this is a *server* decision, never read off the wire. The
/// `can_publish` field on [`LiveKitTokenRequest`] is now inert for capability
/// purposes (kept only so the existing wire body still deserializes); the
/// daemon route derives this value from operator policy and passes it
/// explicitly — the same move OCEAN-160 made when it stopped trusting the wire
/// `yolo` flag and resolved the posture server-side. A caller that does not
/// prove publish entitlement gets [`PublishGrant::Deny`] → a listen-only token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishGrant {
    /// The caller proved entitlement (operator-secret verified on the route, or
    /// an in-process server lane like the call tap): the token may publish.
    Allow,
    /// Default-deny: subscribe/listen only. Unauthorized HTTP callers land here,
    /// so a forged/replayed request can at most listen, never inject media.
    Deny,
}

impl PublishGrant {
    /// Map the grant to the LiveKit `can_publish` bit.
    fn can_publish(self) -> bool {
        matches!(self, PublishGrant::Allow)
    }
}

/// Mint a LiveKit join JWT for `room_id` scoped to the requested participant.
///
/// The identity falls back across participant_id → surface_id → "web-surface"
/// so the token always carries an identity (LiveKit rejects a `room_join` token
/// without one). Subscribe is always granted (you joined to hear the room);
/// `publish` is the server-decided capability — see [`PublishGrant`].
///
/// SECURITY (OCEAN-220, P0): `req.can_publish` is deliberately NOT consulted.
/// Whether this token may publish is decided by the *caller* of this function
/// (the daemon route resolves it from operator policy) and passed as `publish`.
/// This keeps the publish capability off the wire, so a client cannot grant
/// itself media-injection rights into someone else's live call.
pub fn mint_join_token(
    config: &LiveKitTokenConfig,
    room_id: &str,
    req: &LiveKitTokenRequest,
    publish: PublishGrant,
) -> Result<LiveKitTokenResponse, String> {
    let identity = first_non_empty(&[&req.participant_id, &req.surface_id, "web-surface"]);
    let name = if req.display_name.trim().is_empty() {
        identity.clone()
    } else {
        req.display_name.clone()
    };

    let grants = VideoGrants {
        room_join: true,
        room: room_id.to_string(),
        // Server-decided, never `req.can_publish` (OCEAN-220, P0).
        can_publish: publish.can_publish(),
        can_subscribe: req.can_subscribe,
        ..Default::default()
    };

    let token = AccessToken::with_api_key(&config.api_key, &config.api_secret)
        .with_identity(&identity)
        .with_name(&name)
        .with_ttl(TOKEN_TTL)
        .with_grants(grants)
        .to_jwt()
        .map_err(|e| format!("failed to sign LiveKit token: {e}"))?;

    // Tell the client when this token dies so it can refresh ahead of the cliff
    // (OCEAN-240). `now + TOKEN_TTL` mirrors the `with_ttl(TOKEN_TTL)` the SDK
    // stamps onto the JWT `exp` (the SDK's reference "now" is the same call,
    // within sub-ms — immaterial against a 6h horizon). RFC3339, seconds
    // precision, `Z`-suffixed UTC — the exact shape the surface parses.
    let expires_at = (Utc::now() + TOKEN_TTL).to_rfc3339_opts(SecondsFormat::Secs, true);

    Ok(LiveKitTokenResponse {
        ok: true,
        url: config.url.clone(),
        token,
        room: room_id.to_string(),
        expires_at,
    })
}

/// First trimmed-non-empty candidate, owned. `candidates` is ordered by
/// preference; the last is a guaranteed-present fallback.
fn first_non_empty(candidates: &[&str]) -> String {
    candidates
        .iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or("web-surface")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use livekit_api::access_token::TokenVerifier;

    fn config() -> LiveKitTokenConfig {
        LiveKitTokenConfig {
            url: "wss://test.livekit.cloud".into(),
            api_key: "devkey".into(),
            api_secret: "devsecretdevsecretdevsecret0123456789".into(),
        }
    }

    #[test]
    fn request_roundtrips_surface_body() {
        // The exact body the web bridge POSTs (index.html oceanLiveKitConnect).
        let json = r#"{
            "surface_id":"web-surface",
            "participant_id":"web-surface",
            "display_name":"Web Surface",
            "can_publish":true,
            "can_subscribe":true
        }"#;
        let req: LiveKitTokenRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.surface_id, "web-surface");
        assert!(req.can_publish);
        assert!(req.can_subscribe);
    }

    #[test]
    fn request_defaults_publish_subscribe_true() {
        // A sparse body (or the OCEAN-83 shape) still parses, grants default on.
        let req: LiveKitTokenRequest = serde_json::from_str(r#"{}"#).unwrap();
        assert!(req.can_publish);
        assert!(req.can_subscribe);
    }

    #[test]
    fn minted_token_is_verifiable_and_room_scoped() {
        let cfg = config();
        let req = LiveKitTokenRequest {
            participant_id: "alice".into(),
            display_name: "Alice".into(),
            can_publish: true,
            can_subscribe: true,
            ..Default::default()
        };
        let resp = mint_join_token(&cfg, "call:abc", &req, PublishGrant::Allow).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.url, "wss://test.livekit.cloud");
        assert_eq!(resp.room, "call:abc");
        assert!(!resp.token.is_empty());

        // The JWT must verify against the same secret and carry the grants.
        let claims = TokenVerifier::with_api_key(&cfg.api_key, &cfg.api_secret)
            .verify(&resp.token)
            .expect("token verifies");
        assert!(claims.video.room_join);
        assert_eq!(claims.video.room, "call:abc");
        assert_eq!(claims.sub, "alice"); // identity
    }

    #[test]
    fn response_carries_well_formed_expires_at_about_six_hours_ahead() {
        // OCEAN-240 contract: every token response MUST carry `expires_at` in the
        // RFC3339/UTC shape the surface parses, set to ~now + TOKEN_TTL (6h) so
        // the client can pre-emptively refresh before the JWT cliff.
        let cfg = config();
        let req = LiveKitTokenRequest {
            participant_id: "alice".into(),
            ..Default::default()
        };
        let before = Utc::now();
        let resp = mint_join_token(&cfg, "call:abc", &req, PublishGrant::Deny).unwrap();
        let after = Utc::now();

        // Present and well-formed: parses as RFC3339 and is `Z`-suffixed UTC,
        // exactly matching the surface fixtures (e.g. "2026-06-03T22:00:00Z").
        assert!(
            resp.expires_at.ends_with('Z'),
            "expires_at must be Z-suffixed UTC, got {}",
            resp.expires_at
        );
        let parsed = chrono::DateTime::parse_from_rfc3339(&resp.expires_at)
            .expect("expires_at parses as RFC3339")
            .with_timezone(&Utc);

        // ~6h ahead: must fall within [before + TTL, after + TTL]. Allow a
        // one-second slack for the seconds-truncation in the emitted string.
        let lo = before + TOKEN_TTL - chrono::Duration::seconds(1);
        let hi = after + TOKEN_TTL + chrono::Duration::seconds(1);
        assert!(
            parsed >= lo && parsed <= hi,
            "expires_at {parsed} not within ~6h window [{lo}, {hi}]"
        );

        // It must also corroborate the JWT's real `exp` claim — the value we
        // advertise has to match the token we actually signed, not drift from it.
        let claims = TokenVerifier::with_api_key(&cfg.api_key, &cfg.api_secret)
            .verify(&resp.token)
            .unwrap();
        let exp = chrono::DateTime::from_timestamp(claims.exp as i64, 0)
            .expect("exp is a valid instant");
        assert!(
            (parsed - exp).num_seconds().abs() <= 2,
            "advertised expires_at {parsed} must match the JWT exp {exp}"
        );
    }

    #[test]
    fn response_roundtrips_with_expires_at() {
        // The surface declares `expires_at` with no serde default, so a daemon
        // payload that omits it fails to deserialize there. Lock that the wire
        // shape we emit always includes the field (OCEAN-240).
        let cfg = config();
        let resp =
            mint_join_token(&cfg, "r", &LiveKitTokenRequest::default(), PublishGrant::Deny).unwrap();
        let json = serde_json::to_value(&resp).unwrap();
        assert!(
            json.get("expires_at").and_then(|v| v.as_str()).is_some(),
            "serialized response must carry a string expires_at, got {json}"
        );
        let back: LiveKitTokenResponse = serde_json::from_value(json).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn publish_grant_is_server_decided_not_wire() {
        // OCEAN-220 (P0): even a request that screams can_publish=true must NOT
        // get a publish-capable token unless the SERVER passes PublishGrant::Allow.
        let cfg = config();
        let req = LiveKitTokenRequest {
            participant_id: "attacker".into(),
            can_publish: true, // wire says yes — must be ignored
            can_subscribe: true,
            ..Default::default()
        };

        // Server denies publish → listen-only token regardless of the wire flag.
        let denied = mint_join_token(&cfg, "call:victim", &req, PublishGrant::Deny).unwrap();
        let claims = TokenVerifier::with_api_key(&cfg.api_key, &cfg.api_secret)
            .verify(&denied.token)
            .unwrap();
        assert!(
            !claims.video.can_publish,
            "wire can_publish=true must NOT yield a publish grant when the server denies"
        );
        assert!(claims.video.can_subscribe);

        // Server allows publish (the entitled path) → publish-capable token.
        let allowed = mint_join_token(&cfg, "call:victim", &req, PublishGrant::Allow).unwrap();
        let claims = TokenVerifier::with_api_key(&cfg.api_key, &cfg.api_secret)
            .verify(&allowed.token)
            .unwrap();
        assert!(claims.video.can_publish);
    }

    #[test]
    fn subscribe_only_grant_is_honored() {
        let cfg = config();
        let req = LiveKitTokenRequest {
            participant_id: "listener".into(),
            can_publish: false,
            can_subscribe: true,
            ..Default::default()
        };
        // Server denies publish: a subscribe-only token.
        let resp = mint_join_token(&cfg, "room1", &req, PublishGrant::Deny).unwrap();
        let claims = TokenVerifier::with_api_key(&cfg.api_key, &cfg.api_secret)
            .verify(&resp.token)
            .unwrap();
        assert!(!claims.video.can_publish);
        assert!(claims.video.can_subscribe);
    }

    #[test]
    fn identity_falls_back_when_participant_missing() {
        let cfg = config();
        let req = LiveKitTokenRequest {
            surface_id: "surfaceX".into(),
            ..Default::default()
        };
        let resp = mint_join_token(&cfg, "r", &req, PublishGrant::Deny).unwrap();
        let claims = TokenVerifier::with_api_key(&cfg.api_key, &cfg.api_secret)
            .verify(&resp.token)
            .unwrap();
        assert_eq!(claims.sub, "surfaceX");
    }

    #[test]
    fn config_validate_rejects_non_url_host() {
        let mut cfg = config();
        cfg.url = "not-a-url".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_validate_rejects_empty_secret() {
        let mut cfg = config();
        cfg.api_secret = "".into();
        assert!(cfg.validate().is_err());
    }
}
