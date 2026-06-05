//! LiveKit webhook handling — the trigger that tells the daemon a call landed.
//!
//! Inbound calls don't go to a fixed room: the SIP dispatch rule routes each
//! caller into a fresh `call_<caller>_<random>` room. So the daemon can't just
//! join a known room — it must be *told* when one appears. LiveKit POSTs a
//! webhook on room lifecycle events; the daemon verifies it
//! ([`livekit_api::webhooks::WebhookReceiver`], verified API) and decides
//! whether to spin up a [`crate::orchestrator::CallSession`] + tap.
//!
//! This module is the pure decision: given an already-verified event's type +
//! room name, should the daemon join, and is this a call room? No HTTP, no
//! crypto, no native deps — fully unit-testable. The signature verification and
//! room-tap spawn live in the daemon (the receiver needs the API secret; the
//! tap needs native libwebrtc behind `livekit-tap`).

/// What the daemon should do in response to a webhook event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookAction {
    /// A call room appeared — join it and start the pipeline.
    JoinCall { room: String },
    /// A call room closed — tear down its session if still live.
    EndCall { room: String },
    /// Not relevant to call handling (e.g. a non-call room, or track events).
    Ignore,
}

/// The room-name prefix our SIP dispatch rule assigns INBOUND calls. Matches
/// `--caller call` in the `lk sip dispatch create` setup, which yields
/// `call_<caller>_<random>`.
pub const CALL_ROOM_PREFIX: &str = "call_";

/// The room-name prefix `/v1/calls/place` assigns OUTBOUND calls: `call:<uuid>`
/// (see `call_place` in ocean-daemon). LiveKit fires `room_finished` for these
/// the same way, so they must drive call lifecycle too — otherwise a normal
/// hangup on an outbound call never emits `CallEnded` and subscribers are left
/// with a phantom in-progress call.
pub const OUTBOUND_CALL_ROOM_PREFIX: &str = "call:";

/// Is this a call room (inbound `call_…` or outbound `call:…`)? Regular app
/// rooms (e.g. `pm`, `writers`) match neither and are left alone.
fn is_call_room(room_name: &str) -> bool {
    room_name.starts_with(CALL_ROOM_PREFIX) || room_name.starts_with(OUTBOUND_CALL_ROOM_PREFIX)
}

/// Decide what to do given a verified webhook event's type and room name.
///
/// `event` is LiveKit's event string ("room_started", "room_finished",
/// "participant_joined", …). Only room start/finish on a call room — inbound
/// `call_<caller>_<random>` OR outbound `call:<uuid>` — drive session lifecycle;
/// everything else is ignored so we don't double-join or leak phantom calls.
pub fn decide(event: &str, room_name: &str) -> WebhookAction {
    if !is_call_room(room_name) {
        return WebhookAction::Ignore;
    }
    match event {
        "room_started" => WebhookAction::JoinCall {
            room: room_name.to_string(),
        },
        "room_finished" => WebhookAction::EndCall {
            room: room_name.to_string(),
        },
        _ => WebhookAction::Ignore,
    }
}

/// Verify a LiveKit webhook POST and decide what to do.
///
/// `body` is the raw request body; `auth_token` is the value of the
/// `Authorization` header LiveKit sends. The API key/secret must match the
/// project so the signature checks out. Returns the [`WebhookAction`], or an
/// error if verification fails (bad signature, tampered body) — a failed verify
/// must NOT join a room, so the daemon treats Err as "ignore + log".
///
/// Verified against livekit-api 0.5.0: `WebhookReceiver::new(TokenVerifier
/// ::with_api_key(key, secret)).receive(body, token) -> WebhookEvent` whose
/// `event` string + optional `room.name` drive the decision.
pub fn verify_and_decide(
    api_key: &str,
    api_secret: &str,
    body: &str,
    auth_token: &str,
) -> anyhow::Result<WebhookAction> {
    use livekit_api::access_token::TokenVerifier;
    use livekit_api::webhooks::WebhookReceiver;

    let receiver = WebhookReceiver::new(TokenVerifier::with_api_key(api_key, api_secret));
    let event = receiver
        .receive(body, auth_token)
        .map_err(|e| anyhow::anyhow!("webhook verify failed: {e}"))?;

    let room_name = event
        .room
        .as_ref()
        .map(|r| r.name.as_str())
        .unwrap_or_default();
    Ok(decide(&event.event, room_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_started_on_call_room_joins() {
        assert_eq!(
            decide("room_started", "call_+17035551234_aB3x"),
            WebhookAction::JoinCall {
                room: "call_+17035551234_aB3x".into()
            }
        );
    }

    #[test]
    fn room_finished_on_call_room_ends() {
        assert_eq!(
            decide("room_finished", "call_anon_xyz"),
            WebhookAction::EndCall {
                room: "call_anon_xyz".into()
            }
        );
    }

    #[test]
    fn outbound_room_finished_ends_the_call() {
        // /v1/calls/place mints outbound rooms as `call:<uuid>` and emits
        // CallStarted. A normal hangup fires room_finished on that same room, so
        // decide() must return EndCall (not Ignore) or the outbound call lingers
        // as a phantom in-progress call forever.
        let room = format!("call:{}", uuid_like());
        assert_eq!(
            decide("room_finished", &room),
            WebhookAction::EndCall { room: room.clone() }
        );
    }

    #[test]
    fn outbound_room_started_joins() {
        // Symmetry: an outbound `call:<uuid>` room starting is also a JoinCall.
        let room = format!("call:{}", uuid_like());
        assert_eq!(
            decide("room_started", &room),
            WebhookAction::JoinCall { room: room.clone() }
        );
    }

    // A fixed uuid-shaped string so the test doesn't depend on the uuid crate.
    fn uuid_like() -> &'static str {
        "3f2504e0-4f89-41d3-9a0c-0305e82c3301"
    }

    #[test]
    fn non_call_room_is_ignored() {
        // A regular app room (e.g. the surface UI) must not trigger a call tap.
        assert_eq!(decide("room_started", "pm"), WebhookAction::Ignore);
        assert_eq!(decide("room_started", "writers"), WebhookAction::Ignore);
        // A room that merely contains "call" but doesn't have the prefix is also
        // ignored — only the inbound `call_` / outbound `call:` prefixes count.
        assert_eq!(decide("room_finished", "recall-room"), WebhookAction::Ignore);
    }

    #[test]
    fn track_and_participant_events_are_ignored() {
        // Only room start/finish drive lifecycle; joining on every
        // participant_joined would double-spawn the tap.
        assert_eq!(
            decide("participant_joined", "call_x_y"),
            WebhookAction::Ignore
        );
        assert_eq!(decide("track_published", "call_x_y"), WebhookAction::Ignore);
    }

    #[test]
    fn unknown_event_is_ignored() {
        assert_eq!(decide("something_new", "call_x_y"), WebhookAction::Ignore);
    }
}
