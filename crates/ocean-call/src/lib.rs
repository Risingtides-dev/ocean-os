//! Ocean Call Intelligence — daemon-side PSTN call agent.
//!
//! A real phone number, bridged via Twilio SIP into a LiveKit room, that Ocean
//! joins as a server-side participant. Over one shared audio stream it runs two
//! lanes:
//!
//! - **Passive (always on):** transcribe → rolling summary → task detection →
//!   emit [`ocean_core::OceanEvent`] call events onto the daemon SSE rail.
//! - **Active (wake-word gated):** "hey Ocean…" → one ephemeral agent turn →
//!   TTS reply back into the call. Quiet by default.
//!
//! Spec: `docs/superpowers/specs/2026-06-05-ocean-call-intelligence-design.md`.
//!
//! ## Build status
//!
//! The pure, off-device logic is built and tested first behind traits, so most
//! of the crate is green without any cloud account:
//!
//! - [`frame`] — PCM frames + fixed-window re-chunking (room_tap → stt seam).
//!
//! Live-provider adapters (LiveKit `room_tap`, streaming STT, Twilio SIP
//! `sip_bridge`) land on top, and only the real-call leg is gated on John's
//! Twilio upgrade + a LiveKit Cloud account.

pub mod frame;
pub mod orchestrator;
pub mod room_tap;
pub mod session_task;
pub mod sip_bridge;
pub mod speaker;
pub mod stt;
pub mod stt_xai;
pub mod summarizer;
pub mod task_detector;
pub mod token;
pub mod wake;
pub mod webhook;

pub use frame::{FrameChunker, PcmFrame};
pub use orchestrator::{ActiveOutcome, CallSession, CapturingSink, EventSink, SegmentOutcome};
pub use room_tap::TapLifecycle;
pub use session_task::{
    run_call_session, FrameSource, SourceEnd, Transcriber, TurnRunner, UtterancePolicy, Voice,
};
pub use sip_bridge::{normalize_e164, CallBridge, LiveKitSipBridge, PlacedCall, SipConfig};
pub use stt::{SegmentAssembler, SegmentUpdate, SttProvider, TranscriptSegment};
pub use summarizer::{SummaryAction, SummaryPolicy, Summarizer};
pub use task_detector::{detect as detect_task, DetectedTask};
pub use token::{mint_join_token, LiveKitTokenConfig, LiveKitTokenRequest, LiveKitTokenResponse};
pub use wake::{match_wake, WakeAction, WakeGate, WakeMatch};
pub use webhook::{decide as decide_webhook, verify_and_decide, WebhookAction};
