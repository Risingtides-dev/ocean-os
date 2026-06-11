//! SIP bridge — provision the trunk and place the outbound call.
//!
//! This is the unit that actually makes Ocean's phone ring on a real line. It
//! drives LiveKit's server SDK ([`livekit_api::services::sip::SIPClient`]):
//!
//! - **Outbound dial** (Ocean calls a number, e.g. 703-508-1859) →
//!   `create_sip_participant(trunk_id, call_to, room, opts, …)`, verified
//!   against livekit-api 0.5.0 source. With `wait_until_answered = true` it
//!   blocks until the callee picks up or the call fails with a SIP code.
//! - **Trunk provisioning** → `create_sip_outbound_trunk` points LiveKit at
//!   Twilio's termination URI; `create_sip_inbound_trunk` + a dispatch rule
//!   route inbound PSTN calls into a room.
//!
//! The credential-dependent calls live behind the [`CallBridge`] trait so the
//! daemon can mock them in tests; the pure parts (E.164 normalization, config
//! validation) are unit-tested here. **None of this can place a real call
//! until John upgrades Twilio off trial and provides LiveKit Cloud SIP
//! credentials** — but the wiring is built against the verified API, not
//! guessed.

use async_trait::async_trait;

/// Everything needed to bridge calls for one deployment. Sourced from env /
/// config at daemon startup; secrets never logged.
#[derive(Debug, Clone)]
pub struct SipConfig {
    /// LiveKit server host, e.g. "https://my-project.livekit.cloud".
    pub livekit_host: String,
    /// LiveKit API key/secret (server SDK auth).
    pub api_key: String,
    pub api_secret: String,
    /// The provisioned outbound SIP trunk id (from create_sip_outbound_trunk).
    pub outbound_trunk_id: String,
    /// Ocean's own caller-ID number, E.164 (the Twilio number).
    pub caller_number: String,
}

impl SipConfig {
    /// Build a config from environment variables, the way the daemon sources
    /// it at startup. Returns Err naming the first missing/invalid var, so the
    /// place-call route can tell the operator exactly what's not set yet:
    ///
    /// - `LIVEKIT_URL` — the LiveKit Cloud host (https/wss)
    /// - `LIVEKIT_API_KEY`, `LIVEKIT_API_SECRET`
    /// - `OCEAN_CALL_OUTBOUND_TRUNK` — provisioned outbound SIP trunk id
    /// - `OCEAN_CALL_CALLER_NUMBER` — Ocean's E.164 caller id (the Twilio #)
    pub fn from_env() -> Result<Self, String> {
        let var = |k: &str| std::env::var(k).map_err(|_| format!("{k} not set"));
        let config = SipConfig {
            livekit_host: var("LIVEKIT_URL")?,
            api_key: var("LIVEKIT_API_KEY")?,
            api_secret: var("LIVEKIT_API_SECRET")?,
            outbound_trunk_id: var("OCEAN_CALL_OUTBOUND_TRUNK")?,
            caller_number: var("OCEAN_CALL_CALLER_NUMBER")?,
        };
        config.validate()?;
        Ok(config)
    }

    /// Validate the config is complete enough to attempt a call. Returns the
    /// first problem found, or Ok.
    pub fn validate(&self) -> Result<(), String> {
        if self.livekit_host.trim().is_empty() {
            return Err("livekit_host is empty".into());
        }
        if !self.livekit_host.starts_with("http") && !self.livekit_host.starts_with("wss") {
            return Err("livekit_host must be an http(s)/wss URL".into());
        }
        if self.api_key.trim().is_empty() || self.api_secret.trim().is_empty() {
            return Err("LiveKit api_key/api_secret missing".into());
        }
        if self.outbound_trunk_id.trim().is_empty() {
            return Err("outbound_trunk_id missing — provision the trunk first".into());
        }
        if normalize_e164(&self.caller_number).is_none() {
            return Err("caller_number is not a valid E.164 number".into());
        }
        Ok(())
    }
}

/// A placed (or attempted) outbound call.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedCall {
    /// LiveKit participant id for the SIP caller.
    pub participant_id: String,
    /// The room the call was dialed into.
    pub room: String,
    /// The dialed number, E.164.
    pub dialed: String,
}

/// The credential-dependent bridge surface. Mocked in tests; the real impl
/// talks to LiveKit.
#[async_trait]
pub trait CallBridge: Send + Sync {
    /// Dial `number` into `room`, returning once the call is set up (or the
    /// dial fails). `number` is normalized to E.164 by the caller.
    async fn place_call(&self, number: &str, room: &str) -> anyhow::Result<PlacedCall>;
}

/// Normalize a phone number to E.164 (`+<digits>`, 8–15 digits).
///
/// Accepts "(703) 508-1859", "703-508-1859", "+17035081859", "17035081859";
/// assumes US (+1) for a bare 10-digit number. Returns None if it can't be a
/// real E.164 number.
pub fn normalize_e164(input: &str) -> Option<String> {
    let had_plus = input.trim_start().starts_with('+');
    let digits: String = input.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let normalized = if had_plus {
        format!("+{digits}")
    } else if digits.len() == 10 {
        // Bare US number → assume +1.
        format!("+1{digits}")
    } else {
        // 11-digit "1XXXXXXXXXX" and anything else: prefix '+' and let the
        // E.164 length check below decide.
        format!("+{digits}")
    };
    // E.164: '+' then 8..=15 digits.
    let n_digits = normalized.len() - 1;
    if (8..=15).contains(&n_digits) {
        Some(normalized)
    } else {
        None
    }
}

/// The real LiveKit-backed bridge. Constructed from a validated [`SipConfig`].
pub struct LiveKitSipBridge {
    config: SipConfig,
    /// Hard upper bound on a single dial, so an unreachable host can't hang the
    /// caller forever. Longer than the SIP ringing_timeout so a legitimate
    /// answer-wait completes within it.
    connect_timeout: std::time::Duration,
}

impl LiveKitSipBridge {
    pub fn new(config: SipConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            config,
            connect_timeout: std::time::Duration::from_secs(60),
        })
    }

    /// Override the per-dial timeout (default 60s).
    pub fn with_connect_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    pub fn config(&self) -> &SipConfig {
        &self.config
    }
}

#[async_trait]
impl CallBridge for LiveKitSipBridge {
    async fn place_call(&self, number: &str, room: &str) -> anyhow::Result<PlacedCall> {
        use livekit_api::services::sip::{CreateSIPParticipantOptions, SIPClient};

        let dialed = normalize_e164(number)
            .ok_or_else(|| anyhow::anyhow!("not a valid phone number: {number}"))?;

        let client = SIPClient::with_api_key(
            &self.config.livekit_host,
            &self.config.api_key,
            &self.config.api_secret,
        );

        let opts = CreateSIPParticipantOptions {
            participant_identity: format!("sip-caller:{dialed}"),
            participant_name: Some(format!("Call to {dialed}")),
            // Block until the callee answers so a failed dial surfaces here.
            wait_until_answered: Some(true),
            // Bound how long we wait for the callee to pick up, so a no-answer
            // can't wait forever even inside the outer connect timeout.
            ringing_timeout: Some(std::time::Duration::from_secs(45)),
            ..Default::default()
        };

        // Outer safety net: a connect to an unreachable/misconfigured LiveKit
        // host would otherwise hang indefinitely (the SDK's internal HTTP client
        // has no connect timeout we can set). Bound the whole dial so it always
        // returns — ringing_timeout above covers legitimate answer-waiting, so
        // this only trips on a truly stuck request.
        let call = client.create_sip_participant(
            self.config.outbound_trunk_id.clone(),
            dialed.clone(),
            room.to_string(),
            opts,
            None,
        );
        let info = tokio::time::timeout(self.connect_timeout, call)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "LiveKit create_sip_participant timed out after {:?} — host unreachable?",
                    self.connect_timeout
                )
            })?
            .map_err(|e| anyhow::anyhow!("LiveKit create_sip_participant failed: {e}"))?;

        Ok(PlacedCall {
            participant_id: info.participant_identity,
            room: room.to_string(),
            dialed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_us_formats_to_e164() {
        assert_eq!(
            normalize_e164("(703) 508-1859").as_deref(),
            Some("+17035081859")
        );
        assert_eq!(
            normalize_e164("703-508-1859").as_deref(),
            Some("+17035081859")
        );
        assert_eq!(
            normalize_e164("+1 703 508 1859").as_deref(),
            Some("+17035081859")
        );
        assert_eq!(
            normalize_e164("17035081859").as_deref(),
            Some("+17035081859")
        );
    }

    #[test]
    fn rejects_junk_numbers() {
        assert_eq!(normalize_e164(""), None);
        assert_eq!(normalize_e164("abc"), None);
        assert_eq!(normalize_e164("12"), None); // too short
        assert_eq!(normalize_e164(&"9".repeat(20)), None); // too long
    }

    fn good_config() -> SipConfig {
        SipConfig {
            livekit_host: "https://x.livekit.cloud".into(),
            api_key: "key".into(),
            api_secret: "secret".into(),
            outbound_trunk_id: "ST_abc".into(),
            caller_number: "+18338424000".into(),
        }
    }

    #[test]
    fn valid_config_passes() {
        assert!(good_config().validate().is_ok());
    }

    #[test]
    fn missing_trunk_is_rejected() {
        let mut c = good_config();
        c.outbound_trunk_id = "".into();
        assert!(c.validate().unwrap_err().contains("trunk"));
    }

    #[test]
    fn bad_host_is_rejected() {
        let mut c = good_config();
        c.livekit_host = "x.livekit.cloud".into(); // no scheme
        assert!(c.validate().is_err());
    }

    #[test]
    fn bridge_requires_valid_config() {
        let mut c = good_config();
        c.api_key = "".into();
        assert!(LiveKitSipBridge::new(c).is_err());
    }

    // A mock bridge proving the trait is usable without credentials.
    struct MockBridge;
    #[async_trait]
    impl CallBridge for MockBridge {
        async fn place_call(&self, number: &str, room: &str) -> anyhow::Result<PlacedCall> {
            Ok(PlacedCall {
                participant_id: "mock-participant".into(),
                room: room.into(),
                dialed: normalize_e164(number).unwrap_or_default(),
            })
        }
    }

    #[tokio::test]
    async fn mock_bridge_places_call() {
        let bridge = MockBridge;
        let call = bridge
            .place_call("703-508-1859", "call:test")
            .await
            .unwrap();
        assert_eq!(call.dialed, "+17035081859");
        assert_eq!(call.room, "call:test");
    }

    /// Runtime exercise of the REAL LiveKit call path against an unreachable
    /// host. This proves the async/HTTP/Twirp machinery actually *executes* and
    /// fails gracefully — not just that it compiles. Without this, "verified"
    /// meant only type-level. We point at a black-hole address (TEST-NET-1,
    /// RFC 5737, guaranteed unroutable) and assert place_call returns a clean
    /// Err with our wrapped message, never a panic or hang.
    #[tokio::test]
    async fn real_bridge_errors_cleanly_on_unreachable_host() {
        let config = SipConfig {
            // 192.0.2.0/24 is reserved for docs and is unroutable.
            livekit_host: "https://192.0.2.1:7880".into(),
            api_key: "k".into(),
            api_secret: "s".into(),
            outbound_trunk_id: "ST_test".into(),
            caller_number: "+18338424000".into(),
        };
        // Short per-dial timeout so the test proves the bound fires fast.
        let bridge = LiveKitSipBridge::new(config)
            .expect("config valid")
            .with_connect_timeout(std::time::Duration::from_secs(2));

        // Outer test guard: if place_call's own timeout works, this never trips.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            bridge.place_call("703-508-1859", "call:test"),
        )
        .await;

        match result {
            Ok(Ok(_)) => panic!("dial to an unroutable host must not succeed"),
            Ok(Err(e)) => {
                // The call path ran and returned cleanly (timeout or connect
                // error) — exactly what a real failed dial produces, no hang.
                let msg = e.to_string();
                assert!(
                    msg.contains("create_sip_participant"),
                    "error should be our wrapped dial failure, got: {msg}"
                );
            }
            Err(_) => panic!("place_call hung past its own timeout — the bound didn't fire"),
        }
    }
}
