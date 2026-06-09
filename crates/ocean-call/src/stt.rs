//! Streaming speech-to-text: provider trait + interim/final segment assembly.
//!
//! `room_tap` feeds 20ms [`crate::frame::PcmFrame`]s into a streaming STT
//! provider, which emits a flow of [`TranscriptSegment`]s. Streaming STT is
//! *revisionary*: it emits low-latency **interim** hypotheses (`is_final =
//! false`) that get replaced as more audio arrives, then a **final** segment
//! (`is_final = true`) that won't change. The spec's rule is "never block the
//! lane on STT gaps — mark the segment `final:false`", so downstream lanes act
//! on interims for liveness but treat finals as the transcript of record.
//!
//! [`SegmentAssembler`] is the pure, off-device-testable core that tracks the
//! current interim per speaker and decides what the downstream lanes should
//! see. The provider adapters (Deepgram WebSocket first, xAI Grok as a
//! fast-follow) live behind [`SttProvider`] and are the only parts that need a
//! live key — everything here is unit-tested without a network.

use std::collections::HashMap;

use async_trait::async_trait;

use crate::frame::PcmFrame;

/// One transcript segment from streaming STT. Maps 1:1 onto the
/// `CallTranscriptSegment` OceanEvent.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptSegment {
    /// Diarization speaker label (e.g. "speaker_0", or a resolved identity).
    pub speaker: String,
    pub text: String,
    /// Milliseconds from call start when this segment's audio began.
    pub start_ms: u64,
    /// False while STT may still revise this segment; true once settled.
    pub is_final: bool,
}

impl TranscriptSegment {
    pub fn interim(speaker: impl Into<String>, text: impl Into<String>, start_ms: u64) -> Self {
        Self {
            speaker: speaker.into(),
            text: text.into(),
            start_ms,
            is_final: false,
        }
    }

    pub fn final_(speaker: impl Into<String>, text: impl Into<String>, start_ms: u64) -> Self {
        Self {
            speaker: speaker.into(),
            text: text.into(),
            start_ms,
            is_final: true,
        }
    }
}

/// What the assembler tells the downstream lanes to do with an incoming segment.
#[derive(Debug, Clone, PartialEq)]
pub enum SegmentUpdate {
    /// A live interim — render it, but don't commit to summary/task lanes yet.
    Interim(TranscriptSegment),
    /// A settled final — commit it; it is part of the transcript of record.
    Final(TranscriptSegment),
    /// Empty/whitespace segment — ignore entirely.
    Ignore,
}

/// One classified transcript update off a streaming provider, plus any barge-in
/// edge it produced. This is the unit the streaming session loop consumes: the
/// provider's read side hands these out, the loop commits `Final`s to the
/// orchestrator, surfaces `Interim`s, and routes `activity` to the active lane.
///
/// Lives here (always compiled) rather than inside the `deepgram-stt`-gated
/// `live` module so [`crate::session_task::run_call_session_streaming`] can be
/// provider-agnostic and unit-tested against a mock provider with no socket.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamEvent {
    /// The assembler-classified segment (interim / final / ignore).
    pub update: SegmentUpdate,
    /// The speech-activity edge this segment produced, if any. `Some(Onset)` is
    /// the **barge-in** signal — the human just started talking, cut Ocean's TTS.
    pub activity: Option<crate::stt_deepgram::SpeechActivity>,
}

/// A streaming STT provider. Implemented by the Deepgram / xAI adapters; the
/// trait keeps the rest of the crate provider-agnostic and testable with fakes.
///
/// The provider is **push-in / stream-out**: audio frames go in via
/// [`push_frame`](SttProvider::push_frame); classified [`StreamEvent`]s come
/// back out of a channel the constructor returns alongside the provider (e.g.
/// `DeepgramStt::connect` → `(provider, UnboundedReceiver<StreamEvent>)`). The
/// session loop multiplexes the frame source into `push_frame` and that receiver
/// into the orchestrator — see [`crate::session_task::run_call_session_streaming`].
#[async_trait]
pub trait SttProvider: Send + Sync {
    /// Push one PCM frame into the provider's stream.
    async fn push_frame(&self, frame: PcmFrame) -> anyhow::Result<()>;

    /// Signal end of audio so the provider flushes any pending final segment.
    async fn finish(&self) -> anyhow::Result<()>;

    /// Human label for logs / event attribution (e.g. "deepgram").
    fn name(&self) -> &'static str;
}

/// Tracks per-speaker interim state and classifies incoming segments.
///
/// Streaming providers re-emit a speaker's interim repeatedly with growing
/// text; the assembler dedupes identical interims (so we don't spam the lane
/// with the same hypothesis) and recognizes when a final supersedes the
/// interim it was building.
#[derive(Debug, Default)]
pub struct SegmentAssembler {
    /// Last interim text emitted per speaker, to suppress duplicates.
    last_interim: HashMap<String, String>,
}

impl SegmentAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Classify an incoming segment from the provider.
    pub fn ingest(&mut self, seg: TranscriptSegment) -> SegmentUpdate {
        if seg.text.trim().is_empty() {
            return SegmentUpdate::Ignore;
        }
        if seg.is_final {
            // A final settles this speaker's current interim.
            self.last_interim.remove(&seg.speaker);
            SegmentUpdate::Final(seg)
        } else {
            // Suppress an interim identical to the last one we emitted for this
            // speaker — same hypothesis, no new information.
            let prev = self.last_interim.get(&seg.speaker);
            if prev.map(|p| p == &seg.text).unwrap_or(false) {
                return SegmentUpdate::Ignore;
            }
            self.last_interim
                .insert(seg.speaker.clone(), seg.text.clone());
            SegmentUpdate::Interim(seg)
        }
    }

    /// Forget all interim state (e.g. on call end).
    pub fn reset(&mut self) {
        self.last_interim.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_segment_is_ignored() {
        let mut a = SegmentAssembler::new();
        assert_eq!(
            a.ingest(TranscriptSegment::interim("s0", "   ", 0)),
            SegmentUpdate::Ignore
        );
    }

    #[test]
    fn interim_then_final_flow() {
        let mut a = SegmentAssembler::new();
        // Growing interim hypothesis.
        assert_eq!(
            a.ingest(TranscriptSegment::interim("s0", "let's", 100)),
            SegmentUpdate::Interim(TranscriptSegment::interim("s0", "let's", 100))
        );
        assert_eq!(
            a.ingest(TranscriptSegment::interim("s0", "let's ship", 100)),
            SegmentUpdate::Interim(TranscriptSegment::interim("s0", "let's ship", 100))
        );
        // Final settles it.
        assert_eq!(
            a.ingest(TranscriptSegment::final_("s0", "let's ship friday", 100)),
            SegmentUpdate::Final(TranscriptSegment::final_("s0", "let's ship friday", 100))
        );
    }

    #[test]
    fn duplicate_interim_is_suppressed() {
        let mut a = SegmentAssembler::new();
        let first = a.ingest(TranscriptSegment::interim("s0", "hello", 0));
        assert!(matches!(first, SegmentUpdate::Interim(_)));
        // Exact same interim again → ignored, no lane spam.
        let dup = a.ingest(TranscriptSegment::interim("s0", "hello", 0));
        assert_eq!(dup, SegmentUpdate::Ignore);
    }

    #[test]
    fn interims_tracked_per_speaker() {
        let mut a = SegmentAssembler::new();
        // Two speakers with the same interim text are independent.
        assert!(matches!(
            a.ingest(TranscriptSegment::interim("s0", "yeah", 0)),
            SegmentUpdate::Interim(_)
        ));
        assert!(matches!(
            a.ingest(TranscriptSegment::interim("s1", "yeah", 0)),
            SegmentUpdate::Interim(_)
        ));
    }

    #[test]
    fn final_clears_interim_so_next_same_text_interim_emits() {
        let mut a = SegmentAssembler::new();
        a.ingest(TranscriptSegment::interim("s0", "ok", 0));
        a.ingest(TranscriptSegment::final_("s0", "ok", 0));
        // After a final, an interim with the same text is new info again.
        assert!(matches!(
            a.ingest(TranscriptSegment::interim("s0", "ok", 50)),
            SegmentUpdate::Interim(_)
        ));
    }
}
