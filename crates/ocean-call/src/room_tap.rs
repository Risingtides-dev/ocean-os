//! room_tap — join a LiveKit room as a server participant and tap remote audio.
//!
//! Ocean joins the call's LiveKit room read-only (`can_subscribe`, never
//! `can_publish` on the passive lane) and pulls PCM off each remote audio track.
//! The verified API chain (against livekit 0.7.44 / libwebrtc 0.3.35):
//!
//! ```text
//! Room::connect(url, token, RoomOptions) -> (Room, UnboundedReceiver<RoomEvent>)
//!   -> RoomEvent::TrackSubscribed { track: RemoteTrack::Audio(at), participant, .. }
//!   -> NativeAudioStream::new(at.rtc_track(), sample_rate, num_channels, None)
//!   -> stream yields AudioFrame -> PcmFrame -> FrameChunker (20ms windows)
//! ```
//!
//! The token to join is minted by the existing daemon endpoint
//! `POST /v1/rooms/{room_id}/livekit-token` (see ocean-surface's daemon client)
//! with a fixed server identity, `can_publish=false`, `can_subscribe=true`.
//! That token-request shape is pure and tested here. The live connect/subscribe
//! loop is behind the `livekit-tap` feature so the heavy native libwebrtc build
//! doesn't burden the fast-compiling logic core; everything else in the crate
//! builds and tests without it.

use serde::{Deserialize, Serialize};

/// Identity Ocean uses when it joins a call room as the server-side tap.
pub const ROOM_TAP_IDENTITY: &str = "daemon:room-tap";

/// Request body for the daemon's LiveKit token endpoint, shaped for a
/// read-only server participant. Mirrors `LiveKitTokenRequest` in ocean-surface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoomTapTokenRequest {
    pub surface_id: String,
    pub participant_id: String,
    pub display_name: String,
    pub can_publish: bool,
    pub can_subscribe: bool,
}

impl RoomTapTokenRequest {
    /// Build the read-only tap token request: subscribe-only, never publish.
    pub fn read_only() -> Self {
        Self {
            surface_id: ROOM_TAP_IDENTITY.to_string(),
            participant_id: ROOM_TAP_IDENTITY.to_string(),
            display_name: "Ocean (listening)".to_string(),
            can_publish: false,
            can_subscribe: true,
        }
    }
}

/// Token endpoint response (subset we need): the LiveKit URL + JWT to connect.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoomTapTokenResponse {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The daemon path that mints a room token, given a room id.
pub fn token_endpoint_path(room_id: &str) -> String {
    format!("/v1/rooms/{room_id}/livekit-token")
}

/// Why the audio tap stopped. Sent on the tap's lifecycle channel so the
/// caller can emit the right `CallEnded` — a clean hangup vs. a dropped call
/// are different events the operator needs to tell apart, and either way the
/// call lifecycle MUST be closed (no phantom calls left "in progress").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TapLifecycle {
    /// The room closed normally (call ended / both sides hung up).
    Ended,
    /// The connection dropped unexpectedly mid-call; `reason` is LiveKit's
    /// disconnect reason as a string.
    Disconnected { reason: String },
}

impl TapLifecycle {
    /// True if this represents an abnormal drop (vs. a clean end).
    pub fn is_drop(&self) -> bool {
        matches!(self, TapLifecycle::Disconnected { .. })
    }
}

// The live connect + subscribe + PCM-pump loop is behind `livekit-tap` so the
// native libwebrtc build is opt-in and the logic core stays fast to compile.
#[cfg(feature = "livekit-tap")]
pub mod live {
    use livekit::track::RemoteTrack;
    use livekit::webrtc::audio_stream::native::NativeAudioStream;
    use livekit::{Room, RoomEvent, RoomOptions};
    use tokio::sync::mpsc;

    use crate::frame::{FrameChunker, PcmFrame};

    /// Sample rate/channels we request PCM at — 16kHz mono suits STT.
    const TAP_SAMPLE_RATE: i32 = 16_000;
    const TAP_CHANNELS: i32 = 1;

    /// Connect to `url` with `token`, subscribe to remote audio, and forward
    /// 20ms [`PcmFrame`]s on the PCM channel until the room disconnects. The
    /// second returned channel carries exactly one [`TapLifecycle`] when the tap
    /// ends, so the caller always knows the call closed (and why) — preventing
    /// a phantom call stuck "in progress" if the connection drops mid-call.
    ///
    /// Built against verified livekit 0.7.44 APIs. Subscribe-only: the join
    /// token must carry `can_publish=false`.
    pub async fn connect_and_tap(
        url: &str,
        token: &str,
    ) -> anyhow::Result<(
        mpsc::UnboundedReceiver<PcmFrame>,
        tokio::sync::oneshot::Receiver<super::TapLifecycle>,
    )> {
        let (room, mut room_events) =
            Room::connect(url, token, RoomOptions::default()).await?;
        let (tx, rx) = mpsc::unbounded_channel::<PcmFrame>();
        let (life_tx, life_rx) = tokio::sync::oneshot::channel::<super::TapLifecycle>();

        tokio::spawn(async move {
            // Keep the room handle alive for the lifetime of the tap.
            let _room = room;
            let mut lifecycle = super::TapLifecycle::Ended;
            while let Some(event) = room_events.recv().await {
                match event {
                    RoomEvent::TrackSubscribed { track, .. } => {
                        if let RemoteTrack::Audio(audio) = track {
                            let tx = tx.clone();
                            tokio::spawn(pump_track(audio, tx));
                        }
                    }
                    RoomEvent::Disconnected { reason } => {
                        // Mid-call drop: record why and stop. (A normal end
                        // closes the event stream and leaves `Ended`.)
                        lifecycle = super::TapLifecycle::Disconnected {
                            reason: format!("{reason:?}"),
                        };
                        break;
                    }
                    _ => {}
                }
            }
            // Always signal end-of-call so the lifecycle is never left dangling.
            let _ = life_tx.send(lifecycle);
        });

        Ok((rx, life_rx))
    }

    /// Drain one remote audio track into 20ms PCM frames.
    async fn pump_track(
        audio: livekit::track::RemoteAudioTrack,
        tx: mpsc::UnboundedSender<PcmFrame>,
    ) {
        use futures_util::StreamExt;
        // Signature verified against libwebrtc 0.3.35 audio_stream.rs:62 (the
        // livekit re-export): new(track, sample_rate, num_channels) — 3 args.
        let mut stream =
            NativeAudioStream::new(audio.rtc_track(), TAP_SAMPLE_RATE, TAP_CHANNELS);
        let mut chunker = FrameChunker::new_20ms();
        while let Some(frame) = stream.next().await {
            let pcm = PcmFrame::new(
                frame.data.into_owned(),
                frame.sample_rate,
                frame.num_channels,
            );
            for window in chunker.push(&pcm) {
                if tx.send(window).is_err() {
                    return; // receiver dropped — tap is done
                }
            }
        }
        if let Some(tail) = chunker.flush() {
            let _ = tx.send(tail);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_request_never_publishes() {
        let req = RoomTapTokenRequest::read_only();
        assert!(!req.can_publish, "tap must never publish");
        assert!(req.can_subscribe);
        assert_eq!(req.participant_id, ROOM_TAP_IDENTITY);
    }

    #[test]
    fn token_request_roundtrips_json() {
        let req = RoomTapTokenRequest::read_only();
        let json = serde_json::to_string(&req).unwrap();
        let back: RoomTapTokenRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn token_endpoint_path_is_room_scoped() {
        assert_eq!(
            token_endpoint_path("call:abc"),
            "/v1/rooms/call:abc/livekit-token"
        );
    }

    #[test]
    fn token_response_parses_minimal() {
        let resp: RoomTapTokenResponse =
            serde_json::from_str(r#"{"ok":true,"url":"wss://x","token":"jwt"}"#).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.url, "wss://x");
        assert_eq!(resp.token, "jwt");
    }

    #[test]
    fn clean_end_is_not_a_drop() {
        assert!(!TapLifecycle::Ended.is_drop());
    }

    #[test]
    fn disconnect_is_a_drop_and_carries_reason() {
        let life = TapLifecycle::Disconnected {
            reason: "SignalClose".into(),
        };
        assert!(life.is_drop());
        if let TapLifecycle::Disconnected { reason } = life {
            assert_eq!(reason, "SignalClose");
        }
    }
}
