//! The running call-session task — the loop that turns a live call into events.
//!
//! Everything else in this crate is a verified *unit*: [`crate::room_tap`] taps
//! audio, [`crate::stt`] transcribes, [`crate::orchestrator::CallSession`]
//! decides, [`crate::speaker`] speaks. This module is the **spine** that drives
//! them in a long-running task:
//!
//! ```text
//!  FrameSource ──▶ UtteranceBuffer ──(silence / size flush)──▶ Transcriber
//!                                                                   │
//!                                                          TranscriptSegment
//!                                                                   ▼
//!                                                  CallSession::on_segment ──▶ EventSink
//!                                                                   │
//!                                                      ActiveOutcome::Answer(cmd)
//!                                                                   ▼
//!                                              TurnRunner::run ─▶ reply ─▶ Voice::speak
//!                                                                   │
//!                                                       CallSession::mark_replied
//! ```
//!
//! ## Why this is testable without LiveKit / Twilio / an LLM
//!
//! The four sides that touch the outside world are **traits**, so the whole
//! loop runs in a unit test against in-memory fakes:
//!
//! | Trait          | Live impl (gated)                         | Test impl            |
//! |----------------|-------------------------------------------|----------------------|
//! | [`FrameSource`]| LiveKit tap (`livekit-tap` feature)       | a `Vec<PcmFrame>`    |
//! | [`Transcriber`]| xAI batch STT (`xai-stt` feature)         | a scripted map       |
//! | [`TurnRunner`] | the daemon's agent runtime                 | a canned-reply fn    |
//! | [`Voice`]      | TTS → [`crate::speaker`] (`livekit-tap`)  | a capturing buffer   |
//!
//! The daemon constructs the live adapters and spawns [`run_call_session`]; the
//! default `ocean-daemon` build never pulls native WebRTC, because the live
//! adapters live behind the `livekit-tap` feature — this module's contract is
//! feature-free, so it always compiles.

use std::time::Duration;

use async_trait::async_trait;

use crate::frame::PcmFrame;
use crate::orchestrator::{ActiveOutcome, CallSession, EventSink};
use crate::stt::TranscriptSegment;
use crate::stt_xai::UtteranceBuffer;

/// A source of call audio as [`PcmFrame`]s, plus an end-of-call signal.
///
/// The live implementation wraps the LiveKit room tap; tests feed a vec. The
/// loop pulls frames until `next_frame` returns `None`, which means the call
/// ended (the tap's PCM channel closed). `lifecycle` is consulted once the
/// stream ends to tell a clean hangup from a mid-call drop.
#[async_trait]
pub trait FrameSource: Send {
    /// Await the next PCM frame, or `None` when the call's audio has ended.
    async fn next_frame(&mut self) -> Option<PcmFrame>;

    /// Why the source ended. Read after `next_frame` returns `None`. Defaults to
    /// a clean end; the live tap overrides it with the real disconnect reason.
    fn lifecycle(&self) -> SourceEnd {
        SourceEnd::Ended
    }
}

/// How a [`FrameSource`] finished — mirrors [`crate::room_tap::TapLifecycle`]
/// without forcing every source (or test) to depend on the livekit-gated type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceEnd {
    /// Clean end: the call hung up normally.
    Ended,
    /// Abnormal drop mid-call; `reason` is the transport's disconnect reason.
    Dropped { reason: String },
}

#[cfg(feature = "livekit-tap")]
impl From<crate::room_tap::TapLifecycle> for SourceEnd {
    fn from(life: crate::room_tap::TapLifecycle) -> Self {
        match life {
            crate::room_tap::TapLifecycle::Ended => SourceEnd::Ended,
            crate::room_tap::TapLifecycle::Disconnected { reason } => {
                SourceEnd::Dropped { reason }
            }
        }
    }
}

/// Transcribes one buffered utterance (a WAV blob) into at most one final
/// [`TranscriptSegment`]. The live impl POSTs to xAI's batch endpoint; tests
/// return scripted segments keyed off call order.
///
/// Returning `Ok(None)` means "this utterance had no speech" (silence / noise)
/// and is skipped, exactly like the live endpoint returning empty text.
#[async_trait]
pub trait Transcriber: Send {
    /// Transcribe `wav` (a self-describing WAV container). `start_ms` is the
    /// call-relative time of the utterance's first frame, stamped onto the
    /// resulting segment so downstream ordering is preserved.
    async fn transcribe(
        &mut self,
        wav: Vec<u8>,
        start_ms: u64,
    ) -> anyhow::Result<Option<TranscriptSegment>>;
}

/// Runs one ephemeral agent turn over a wake command and returns the reply text
/// to speak. The daemon impl drives the agent runtime; tests return a canned
/// string. An `Err` aborts just this answer (logged), never the whole call.
#[async_trait]
pub trait TurnRunner: Send {
    async fn run(&mut self, command: &str) -> anyhow::Result<String>;
}

/// Speaks Ocean's reply back into the call. The live impl synthesizes TTS PCM
/// and pushes it via [`crate::speaker`]; tests capture the text. An `Err` is
/// logged and the call continues (a failed reply must not drop the call).
#[async_trait]
pub trait Voice: Send {
    async fn speak(&mut self, text: &str) -> anyhow::Result<()>;
}

/// When to cut the current [`UtteranceBuffer`] and ship it to STT.
///
/// The live tap delivers a continuous 20ms frame stream with no gaps, so the
/// loop can't wait for "silence" off the channel itself — it flushes on a
/// **max duration** (an utterance got long enough to transcribe) and on a
/// **frame-gap timeout** (no frame arrived for a beat → the speaker paused).
/// Both are configurable so a test can flush deterministically.
#[derive(Debug, Clone)]
pub struct UtterancePolicy {
    /// Flush once the buffer holds at least this many ms of audio, so a long
    /// monologue still produces timely interim transcripts.
    pub max_utterance_ms: u64,
    /// Flush if no new frame arrives within this gap — a natural pause that
    /// ends an utterance. Drives the batch-STT cadence off the tap stream.
    pub silence_gap_ms: u64,
    /// How often to drive [`CallSession::on_tick`] (debounced summaries).
    pub tick_interval_ms: u64,
}

impl Default for UtterancePolicy {
    fn default() -> Self {
        Self {
            // ~3s of speech is a comfortable batch-STT chunk; long enough to be
            // a coherent utterance, short enough to keep transcripts flowing.
            max_utterance_ms: 3_000,
            // ~700ms of dead air ends an utterance (typical inter-sentence gap).
            silence_gap_ms: 700,
            tick_interval_ms: 1_000,
        }
    }
}

/// Instruction prepended to a call-segment transcript when running the summary
/// turn. The active-lane [`TurnRunner`] takes only a prompt (no separate system
/// field), so the directive rides at the head of the prompt — the transcript
/// follows under a clear delimiter. Kept tight so the model returns the summary
/// and nothing else, suitable to drop straight onto the operator rail.
const SUMMARY_INSTRUCTION: &str = "Summarize this call segment in 2-3 sentences. \
Reply with the summary only — no preamble, no labels, no quoting the transcript.";

/// Build the summary-turn prompt from the raw joined transcript.
fn summary_prompt(transcript: &str) -> String {
    format!("{SUMMARY_INSTRUCTION}\n\nTranscript:\n{}", transcript.trim())
}

/// Estimate the buffered audio duration in ms from sample count + format.
fn buffered_ms(samples: usize, sample_rate: u32, channels: u16) -> u64 {
    if sample_rate == 0 {
        return 0;
    }
    let frames = samples as u64 / channels.max(1) as u64;
    (frames * 1000) / sample_rate as u64
}

/// The running call-session loop. Drives audio → STT → orchestrator → TTS for
/// one call, emitting events through `sink`, until `source` ends.
///
/// This is the function the daemon spawns per call. It is fully deterministic
/// given its trait inputs, so the test below drives a whole call — start →
/// transcript → task → summary → wake → agent-spoke → end — with zero I/O.
///
/// Contract:
/// - emits `CallStarted` up front and `CallEnded` (with measured duration) once
///   the source ends — a phantom "in progress" call can never be left behind;
/// - buffers frames into utterances, flushing to `transcribe` on size/gap, then
///   feeds each resulting segment to [`CallSession::on_segment`];
/// - when the debounced summarizer fires (via `on_segment`'s
///   [`crate::orchestrator::SegmentOutcome::summary_due`] or `on_tick` on
///   silence), runs an agent turn over the joined transcript and emits
///   [`ocean_core::OceanEvent::CallSummaryUpdated`] with the *LLM summary* — the
///   orchestrator never emits a raw-transcript "summary" itself;
/// - on [`ActiveOutcome::Answer`], runs the turn, speaks the reply, emits
///   [`ocean_core::OceanEvent::CallAgentSpoke`], and starts the wake cooldown;
/// - calls [`CallSession::on_tick`] on `policy.tick_interval_ms`.
#[allow(clippy::too_many_arguments)]
pub async fn run_call_session<S, T, R, V, K>(
    mut session: CallSession,
    mut source: S,
    mut transcriber: T,
    mut runner: R,
    mut voice: V,
    mut sink: K,
    room_id: String,
    participants: Vec<String>,
    policy: UtterancePolicy,
    clock: impl Fn() -> u64 + Send,
) where
    S: FrameSource,
    T: Transcriber,
    R: TurnRunner,
    V: Voice,
    K: EventSink,
{
    let started_ms = clock();
    session.start(&mut sink, &room_id, participants);

    let mut buffer = UtteranceBuffer::new();
    let mut buffered_samples: usize = 0;
    let mut sample_rate: u32 = 0;
    let mut channels: u16 = 1;

    let mut tick = tokio::time::interval(Duration::from_millis(policy.tick_interval_ms.max(1)));
    // The first interval tick fires immediately; consume it so a tick can't
    // pre-empt the very first frame in a tight test.
    tick.tick().await;

    let gap = Duration::from_millis(policy.silence_gap_ms.max(1));

    loop {
        tokio::select! {
            // Bias toward draining audio first so a backlog of frames is
            // transcribed before a tick fires a (likely empty) summary.
            biased;

            maybe_frame = source.next_frame() => {
                match maybe_frame {
                    Some(frame) => {
                        if sample_rate == 0 {
                            sample_rate = frame.sample_rate;
                            channels = frame.num_channels.max(1) as u16;
                        }
                        buffered_samples += frame.data.len();
                        let now = clock();
                        buffer.push(&frame, now.saturating_sub(started_ms));

                        // Size-based flush: enough speech buffered to transcribe.
                        if buffered_ms(buffered_samples, sample_rate, channels)
                            >= policy.max_utterance_ms
                        {
                            flush_utterance(
                                &mut buffer,
                                &mut buffered_samples,
                                &mut transcriber,
                                &mut session,
                                &mut runner,
                                &mut voice,
                                &mut sink,
                                started_ms,
                                &clock,
                            )
                            .await;
                        }
                    }
                    None => break, // source ended — call is over
                }
            }

            // Frame-gap timeout: a pause long enough to end an utterance. Only
            // meaningful when something is buffered.
            _ = tokio::time::sleep(gap), if !buffer.is_empty() => {
                flush_utterance(
                    &mut buffer,
                    &mut buffered_samples,
                    &mut transcriber,
                    &mut session,
                    &mut runner,
                    &mut voice,
                    &mut sink,
                    started_ms,
                    &clock,
                )
                .await;
            }

            _ = tick.tick() => {
                let now = clock().saturating_sub(started_ms);
                // A debounced silence-summary may be due; the orchestrator hands
                // back the raw transcript and we run the real summary turn.
                if let Some(transcript) = session.on_tick(now) {
                    run_summary(transcript, &mut runner, &mut sink, now).await;
                }
            }
        }
    }

    // Drain any trailing buffered audio so the last words aren't lost.
    if !buffer.is_empty() {
        flush_utterance(
            &mut buffer,
            &mut buffered_samples,
            &mut transcriber,
            &mut session,
            &mut runner,
            &mut voice,
            &mut sink,
            started_ms,
            &clock,
        )
        .await;
    }

    let end = source.lifecycle();
    match &end {
        SourceEnd::Ended => tracing::info!(%room_id, "call tap ended cleanly"),
        SourceEnd::Dropped { reason } => {
            tracing::warn!(%room_id, %reason, "call tap dropped mid-call");
        }
    }
    let duration_ms = clock().saturating_sub(started_ms);
    session.end(&mut sink, duration_ms);
}

/// Take the buffered utterance, transcribe it, and run the resulting segment
/// (if any) through the orchestrator — including the wake/answer→TTS lane.
#[allow(clippy::too_many_arguments)]
async fn flush_utterance<T, R, V, K>(
    buffer: &mut UtteranceBuffer,
    buffered_samples: &mut usize,
    transcriber: &mut T,
    session: &mut CallSession,
    runner: &mut R,
    voice: &mut V,
    sink: &mut K,
    started_ms: u64,
    clock: &(impl Fn() -> u64 + Send),
) where
    T: Transcriber,
    R: TurnRunner,
    V: Voice,
    K: EventSink,
{
    let Some((wav, start_ms)) = buffer.take_wav() else {
        return;
    };
    *buffered_samples = 0;

    let seg = match transcriber.transcribe(wav, start_ms).await {
        Ok(Some(seg)) => seg,
        Ok(None) => return, // silence / no speech
        Err(e) => {
            // A transcription failure drops one utterance, never the call.
            tracing::warn!(error = %e, "call STT failed for one utterance; skipping");
            return;
        }
    };

    let now = clock().saturating_sub(started_ms);
    let outcome = session.on_segment(seg, now, sink);
    // Summary lane: the orchestrator decided a summary is due and handed back the
    // raw transcript. Run a real agent turn over it and emit the LLM summary —
    // never the raw join. Done before the answer so the rolling summary reflects
    // the segment that may also have triggered the wake answer.
    if let Some(transcript) = outcome.summary_due {
        run_summary(transcript, runner, sink, now).await;
    }
    if let ActiveOutcome::Answer(command) = outcome.active {
        run_answer(command, session, runner, voice, sink, started_ms, clock).await;
    }
}

/// Run one wake-triggered answer: agent turn → speak → CallAgentSpoke → cooldown.
async fn run_answer<R, V, K>(
    command: String,
    session: &mut CallSession,
    runner: &mut R,
    voice: &mut V,
    sink: &mut K,
    started_ms: u64,
    clock: &(impl Fn() -> u64 + Send),
) where
    R: TurnRunner,
    V: Voice,
    K: EventSink,
{
    let reply = match runner.run(&command).await {
        Ok(reply) => reply,
        Err(e) => {
            tracing::warn!(error = %e, command, "agent turn failed for wake answer");
            // Still start the cooldown so a failed answer doesn't re-trigger in
            // a tight loop on the same utterance echo.
            session.mark_replied(clock().saturating_sub(started_ms));
            return;
        }
    };
    let reply = reply.trim().to_string();
    if !reply.is_empty() {
        if let Err(e) = voice.speak(&reply).await {
            tracing::warn!(error = %e, "TTS playback failed for wake answer");
        }
        // Emit CallAgentSpoke regardless of TTS transport success: the operator
        // rail should show what Ocean said even if the audio leg degraded.
        sink.emit(ocean_core::OceanEvent::CallAgentSpoke { text: reply });
    }
    session.mark_replied(clock().saturating_sub(started_ms));
}

/// Run one debounced summary turn: agent turn over the joined transcript →
/// emit [`OceanEvent::CallSummaryUpdated`] with the LLM summary.
///
/// This is what makes the rolling summary a *real* summary: the orchestrator
/// only debounces and hands back the raw joined transcript; the actual 2-3
/// sentence summary is produced by an agent turn here, exactly mirroring how
/// [`run_answer`] turns a wake command into spoken text.
///
/// Failure policy: a failed or empty summary turn is logged and *skipped* — we
/// never fall back to emitting the raw transcript as if it were a summary (that
/// is the very bug this fixes), and a summary failure must never drop the call.
/// The previous summary simply stands until the next one succeeds.
async fn run_summary<R, K>(transcript: String, runner: &mut R, sink: &mut K, as_of_ms: u64)
where
    R: TurnRunner,
    K: EventSink,
{
    if transcript.trim().is_empty() {
        return;
    }
    let prompt = summary_prompt(&transcript);
    let summary = match runner.run(&prompt).await {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            tracing::warn!(error = %e, "agent turn failed for call summary; keeping prior summary");
            return;
        }
    };
    if summary.is_empty() {
        tracing::warn!("call summary turn returned empty text; keeping prior summary");
        return;
    }
    sink.emit(ocean_core::OceanEvent::CallSummaryUpdated {
        summary,
        as_of_ms,
    });
}

// ---------------------------------------------------------------------------
// Live adapters — the concrete trait impls that bind the loop to LiveKit + xAI
// + TTS. Gated behind `livekit-tap` because publishing/subscribing PCM pulls
// native libwebrtc; the default daemon build never compiles this, so it stays
// fast and credential-free. The loop above is feature-free and always builds.
// ---------------------------------------------------------------------------
#[cfg(feature = "livekit-tap")]
pub mod live {
    use super::*;
    use std::sync::Arc;

    use livekit::Room;
    use tokio::sync::mpsc;

    /// [`FrameSource`] over the live LiveKit room tap. Pulls 20ms PCM frames off
    /// the tap's channel and surfaces the room's disconnect reason as the
    /// lifecycle. Construct via [`connect`], which joins the room (publish-capable
    /// so the active lane can also speak) and returns this plus the shared room.
    pub struct LiveKitFrameSource {
        pcm_rx: mpsc::UnboundedReceiver<PcmFrame>,
        life_rx: Option<tokio::sync::oneshot::Receiver<crate::room_tap::TapLifecycle>>,
        end: SourceEnd,
    }

    impl LiveKitFrameSource {
        /// Join `url` with `token`, returning the frame source and the shared
        /// `Arc<Room>` the [`LiveKitVoice`] publishes into. The token should
        /// carry `can_publish=true` so the active lane's voice track is accepted.
        pub async fn connect(url: &str, token: &str) -> anyhow::Result<(Self, Arc<Room>)> {
            let (room, pcm_rx, life_rx) =
                crate::room_tap::live::connect_and_tap_with_room(url, token).await?;
            Ok((
                Self {
                    pcm_rx,
                    life_rx: Some(life_rx),
                    end: SourceEnd::Ended,
                },
                room,
            ))
        }
    }

    #[async_trait]
    impl FrameSource for LiveKitFrameSource {
        async fn next_frame(&mut self) -> Option<PcmFrame> {
            match self.pcm_rx.recv().await {
                Some(frame) => Some(frame),
                None => {
                    // Channel closed → the tap ended. Capture why (if the
                    // lifecycle oneshot already resolved) for the CallEnded.
                    if let Some(rx) = self.life_rx.take() {
                        if let Ok(life) = rx.await {
                            self.end = SourceEnd::from(life);
                        }
                    }
                    None
                }
            }
        }

        fn lifecycle(&self) -> SourceEnd {
            self.end.clone()
        }
    }

    /// [`Transcriber`] backed by the verified xAI batch STT endpoint. Each
    /// flushed utterance WAV is POSTed; the resulting text becomes one final
    /// segment. Needs `XAI_API_KEY` (and the `xai-stt` feature for the live
    /// HTTP call).
    #[cfg(feature = "xai-stt")]
    pub struct XaiTranscriber {
        client: reqwest::Client,
        api_key: String,
    }

    #[cfg(feature = "xai-stt")]
    impl XaiTranscriber {
        pub fn new(api_key: impl Into<String>) -> Self {
            Self {
                client: reqwest::Client::new(),
                api_key: api_key.into(),
            }
        }
    }

    #[cfg(feature = "xai-stt")]
    #[async_trait]
    impl Transcriber for XaiTranscriber {
        async fn transcribe(
            &mut self,
            wav: Vec<u8>,
            start_ms: u64,
        ) -> anyhow::Result<Option<TranscriptSegment>> {
            crate::stt_xai::live::transcribe_wav(&self.client, &self.api_key, wav, start_ms).await
        }
    }

    /// Turns reply text into TTS PCM to push into the call. This is the one seam
    /// that has no verified provider in-repo yet (the active-lane TTS leg is the
    /// last thing gated on a provider choice), so it stays a trait: wire a real
    /// synthesizer (xAI/ElevenLabs/Cartesia → 16kHz mono PCM) behind it and the
    /// [`LiveKitVoice`] below speaks for real, no other change.
    #[async_trait]
    pub trait TtsSynth: Send {
        /// Synthesize `text` to a single 16kHz-mono PCM utterance.
        async fn synth(&mut self, text: &str) -> anyhow::Result<PcmFrame>;
    }

    /// [`Voice`] that publishes a local voice track into the call room and pushes
    /// synthesized TTS PCM through it via [`crate::speaker`]. The voice track is
    /// published lazily on first speak so a call that never triggers the active
    /// lane never publishes anything.
    pub struct LiveKitVoice<S: TtsSynth> {
        room: Arc<Room>,
        synth: S,
        source: Option<livekit::webrtc::audio_source::native::NativeAudioSource>,
        sample_rate: u32,
        num_channels: u32,
    }

    impl<S: TtsSynth> LiveKitVoice<S> {
        /// Create a voice over `room`, synthesizing with `synth`. PCM is 16kHz
        /// mono to match the tap + STT lane.
        pub fn new(room: Arc<Room>, synth: S) -> Self {
            Self {
                room,
                synth,
                source: None,
                sample_rate: 16_000,
                num_channels: 1,
            }
        }

        async fn ensure_source(
            &mut self,
        ) -> anyhow::Result<&livekit::webrtc::audio_source::native::NativeAudioSource> {
            if self.source.is_none() {
                let src = crate::speaker::live::publish_voice_track(
                    &self.room,
                    self.sample_rate,
                    self.num_channels,
                )
                .await?;
                self.source = Some(src);
            }
            Ok(self.source.as_ref().expect("source just set"))
        }
    }

    #[async_trait]
    impl<S: TtsSynth> Voice for LiveKitVoice<S> {
        async fn speak(&mut self, text: &str) -> anyhow::Result<()> {
            let pcm = self.synth.synth(text).await?;
            let source = self.ensure_source().await?;
            crate::speaker::live::speak_pcm(source, pcm).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::CapturingSink;
    use crate::summarizer::{SummaryPolicy, Summarizer};
    use crate::wake::WakeGate;
    use ocean_core::OceanEvent;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    /// A frame source backed by a fixed list of frames, then "ends".
    struct VecFrames {
        frames: VecDeque<PcmFrame>,
        end: SourceEnd,
    }

    impl VecFrames {
        fn new(frames: Vec<PcmFrame>) -> Self {
            Self {
                frames: frames.into(),
                end: SourceEnd::Ended,
            }
        }
        fn dropped(frames: Vec<PcmFrame>, reason: &str) -> Self {
            Self {
                frames: frames.into(),
                end: SourceEnd::Dropped {
                    reason: reason.into(),
                },
            }
        }
    }

    #[async_trait]
    impl FrameSource for VecFrames {
        async fn next_frame(&mut self) -> Option<PcmFrame> {
            self.frames.pop_front()
        }
        fn lifecycle(&self) -> SourceEnd {
            self.end.clone()
        }
    }

    /// A transcriber that yields scripted texts in order, one per flushed
    /// utterance. `None` entries model a silent utterance.
    struct ScriptedStt {
        scripts: VecDeque<Option<String>>,
    }

    impl ScriptedStt {
        fn new(scripts: Vec<Option<&str>>) -> Self {
            Self {
                scripts: scripts.into_iter().map(|s| s.map(String::from)).collect(),
            }
        }
    }

    #[async_trait]
    impl Transcriber for ScriptedStt {
        async fn transcribe(
            &mut self,
            _wav: Vec<u8>,
            start_ms: u64,
        ) -> anyhow::Result<Option<TranscriptSegment>> {
            match self.scripts.pop_front() {
                Some(Some(text)) => Ok(Some(TranscriptSegment::final_("caller", text, start_ms))),
                _ => Ok(None),
            }
        }
    }

    /// A turn runner that records every prompt it saw and replies. The same
    /// [`TurnRunner`] serves both the wake-answer lane and the summary lane, so
    /// the mock distinguishes them by the [`SUMMARY_INSTRUCTION`] prefix and
    /// returns a dedicated `summary_reply` for summary turns — letting a test
    /// assert the emitted `CallSummaryUpdated` carries the *runner's* summary,
    /// not the raw transcript, while leaving the spoken wake reply unchanged.
    struct CannedRunner {
        /// Reply for wake-answer turns (spoken back to the call).
        reply: String,
        /// Reply for summary turns; falls back to `reply` if unset.
        summary_reply: Option<String>,
        /// Every prompt the runner was handed, in order.
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl CannedRunner {
        fn new(reply: &str, seen: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                reply: reply.into(),
                summary_reply: None,
                seen,
            }
        }
        fn with_summary(mut self, summary_reply: &str) -> Self {
            self.summary_reply = Some(summary_reply.into());
            self
        }
    }

    #[async_trait]
    impl TurnRunner for CannedRunner {
        async fn run(&mut self, command: &str) -> anyhow::Result<String> {
            self.seen.lock().unwrap().push(command.to_string());
            if command.starts_with(SUMMARY_INSTRUCTION) {
                Ok(self
                    .summary_reply
                    .clone()
                    .unwrap_or_else(|| self.reply.clone()))
            } else {
                Ok(self.reply.clone())
            }
        }
    }

    /// A voice that records what it was asked to speak.
    #[derive(Clone, Default)]
    struct CapturingVoice {
        spoken: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Voice for CapturingVoice {
        async fn speak(&mut self, text: &str) -> anyhow::Result<()> {
            self.spoken.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    fn frame_3s() -> PcmFrame {
        // 16kHz mono, 3000ms = 48_000 samples → flushes on the 3s size policy.
        PcmFrame::new(vec![1i16; 48_000], 16_000, 1)
    }

    fn session(muted: bool) -> CallSession {
        CallSession::new(
            "call-test",
            Summarizer::new(SummaryPolicy {
                every_n_segments: 2,
                silence_ms: 5_000,
            }),
            WakeGate::new(muted, 2_000),
        )
    }

    /// A monotonically-advancing fake clock so durations and ticks are
    /// deterministic without real wall-clock time.
    fn step_clock(start: u64, step: u64) -> impl Fn() -> u64 {
        let t = Arc::new(AtomicU64::new(start));
        move || t.fetch_add(step, Ordering::SeqCst)
    }

    fn ev_types(sink: &CapturingSink) -> Vec<&'static str> {
        sink.events
            .iter()
            .map(|e| match e {
                OceanEvent::CallStarted { .. } => "started",
                OceanEvent::CallTranscriptSegment { .. } => "segment",
                OceanEvent::CallSummaryUpdated { .. } => "summary",
                OceanEvent::CallTaskDetected { .. } => "task",
                OceanEvent::CallWakeTriggered { .. } => "wake",
                OceanEvent::CallAgentSpoke { .. } => "spoke",
                OceanEvent::CallEnded { .. } => "ended",
                _ => "other",
            })
            .collect()
    }

    #[tokio::test]
    async fn empty_call_emits_started_and_ended() {
        let mut out = CapturingSink::default();
        run_call_session(
            session(false),
            VecFrames::new(vec![]),
            ScriptedStt::new(vec![]),
            CannedRunner::new("hi", Default::default()),
            CapturingVoice::default(),
            &mut out,
            "call:room".into(),
            vec!["sip:+1700".into()],
            UtterancePolicy::default(),
            step_clock(1_000, 10),
        )
        .await;

        assert_eq!(ev_types(&out), vec!["started", "ended"]);
    }

    #[tokio::test]
    async fn full_call_drives_orchestrator_events_end_to_end() {
        // Three utterances: a task commitment, a normal line (so the 2nd final
        // crosses the summary threshold), and a wake command.
        let frames = vec![frame_3s(), frame_3s(), frame_3s()];
        let stt = ScriptedStt::new(vec![
            Some("I'll send the master to Atlantic tonight"),
            Some("the release is locked for friday"),
            Some("hey Ocean what did we agree to"),
        ]);
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let voice = CapturingVoice::default();
        let spoken = voice.spoken.clone();
        let mut out = CapturingSink::default();

        run_call_session(
            session(false),
            VecFrames::new(frames),
            stt,
            CannedRunner::new("You agreed to send the master.", seen.clone())
                .with_summary("Caller will send the master and the release is locked for Friday."),
            voice,
            &mut out,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        )
        .await;

        let types = ev_types(&out);
        // Lifecycle brackets the call.
        assert_eq!(types.first(), Some(&"started"));
        assert_eq!(types.last(), Some(&"ended"));
        // Passive lane produced a transcript, a detected task, and a summary.
        assert!(types.contains(&"segment"), "got {types:?}");
        assert!(types.contains(&"task"), "got {types:?}");
        assert!(types.contains(&"summary"), "got {types:?}");
        // Active lane: wake fired, agent ran over the command, Ocean spoke.
        assert!(types.contains(&"wake"), "got {types:?}");
        assert!(types.contains(&"spoke"), "got {types:?}");
        // The runner was driven for BOTH lanes: first the summary turn over the
        // joined transcript (2nd final crossed the threshold), then the wake
        // command. The summary prompt carries the instruction + raw transcript.
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "runner drives summary + answer; got {seen:?}");
        assert!(
            seen[0].starts_with(SUMMARY_INSTRUCTION),
            "first turn is the summary turn; got {:?}",
            seen[0]
        );
        assert!(
            seen[0].contains("I'll send the master to Atlantic tonight")
                && seen[0].contains("the release is locked for friday"),
            "summary turn must carry the joined raw transcript; got {:?}",
            seen[0]
        );
        assert_eq!(
            seen[1], "what did we agree to",
            "second turn is the wake command, stripped"
        );
        // Only the wake answer is spoken — a summary is never spoken aloud.
        assert_eq!(
            spoken.lock().unwrap().as_slice(),
            &["You agreed to send the master.".to_string()],
            "voice should speak the agent reply, not the summary"
        );
        // The spoke event carries the reply text.
        let spoke = out
            .events
            .iter()
            .find(|e| matches!(e, OceanEvent::CallAgentSpoke { .. }))
            .unwrap();
        if let OceanEvent::CallAgentSpoke { text } = spoke {
            assert_eq!(text, "You agreed to send the master.");
        }
        // The summary event carries the LLM summary (the runner's summary_reply),
        // NOT the raw joined transcript — the whole point of OCEAN-CALL.
        let summary = out
            .events
            .iter()
            .find_map(|e| match e {
                OceanEvent::CallSummaryUpdated { summary, .. } => Some(summary.clone()),
                _ => None,
            })
            .expect("a CallSummaryUpdated must be emitted");
        assert_eq!(
            summary,
            "Caller will send the master and the release is locked for Friday."
        );
        assert!(
            !summary.contains("the release is locked for friday"),
            "summary must be the LLM output, not the raw lowercase transcript join"
        );
    }

    #[tokio::test]
    async fn muted_call_never_speaks_even_on_wake() {
        let frames = vec![frame_3s()];
        let stt = ScriptedStt::new(vec![Some("hey Ocean summarize the call")]);
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let voice = CapturingVoice::default();
        let spoken = voice.spoken.clone();
        let mut out = CapturingSink::default();

        run_call_session(
            session(true), // muted
            VecFrames::new(frames),
            stt,
            CannedRunner::new("should not be spoken", seen.clone()),
            voice,
            &mut out,
            "call:sensitive".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        )
        .await;

        let types = ev_types(&out);
        // Transcript still flows (passive lane is always on)...
        assert!(types.contains(&"segment"), "got {types:?}");
        // ...but no wake, no agent turn, no speech on a muted call.
        assert!(!types.contains(&"wake"), "got {types:?}");
        assert!(!types.contains(&"spoke"), "got {types:?}");
        assert!(
            seen.lock().unwrap().is_empty(),
            "muted call must not run a turn"
        );
        assert!(
            spoken.lock().unwrap().is_empty(),
            "muted call must not speak"
        );
    }

    #[tokio::test]
    async fn silent_utterance_is_skipped() {
        // STT returns None (silence) for the only utterance → no segment event.
        let frames = vec![frame_3s()];
        let stt = ScriptedStt::new(vec![None]);
        let mut out = CapturingSink::default();

        run_call_session(
            session(false),
            VecFrames::new(frames),
            stt,
            CannedRunner::new("x", Default::default()),
            CapturingVoice::default(),
            &mut out,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        )
        .await;

        let types = ev_types(&out);
        assert_eq!(
            types,
            vec!["started", "ended"],
            "silence yields no transcript"
        );
    }

    #[tokio::test]
    async fn dropped_source_still_closes_the_call() {
        // A mid-call drop must still emit CallEnded (no phantom in-progress call).
        let mut out = CapturingSink::default();
        run_call_session(
            session(false),
            VecFrames::dropped(vec![], "SignalClose"),
            ScriptedStt::new(vec![]),
            CannedRunner::new("x", Default::default()),
            CapturingVoice::default(),
            &mut out,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(1_000, 50),
        )
        .await;

        assert!(ev_types(&out).contains(&"ended"), "dropped call must close");
    }

    #[tokio::test]
    async fn trailing_audio_is_flushed_at_end() {
        // One short frame (under the 3s size flush) followed by source end: the
        // tail must still be transcribed via the end-of-loop drain.
        let short = PcmFrame::new(vec![1i16; 1_600], 16_000, 1); // 100ms
        let stt = ScriptedStt::new(vec![Some("just a quick note")]);
        let mut out = CapturingSink::default();

        run_call_session(
            session(false),
            VecFrames::new(vec![short]),
            stt,
            CannedRunner::new("x", Default::default()),
            CapturingVoice::default(),
            &mut out,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        )
        .await;

        assert!(
            ev_types(&out).contains(&"segment"),
            "trailing sub-threshold audio should still be transcribed at end"
        );
    }

    #[tokio::test]
    async fn summary_turn_emits_llm_summary_not_raw_transcript() {
        // Two finals cross the every_n_segments=2 threshold → one summary turn.
        // Neither line is a task or wake command, so the ONLY runner call is the
        // summary turn — letting us assert the transcript it received and that the
        // emitted summary is the runner's reply, not the raw join.
        let frames = vec![frame_3s(), frame_3s()];
        let stt = ScriptedStt::new(vec![
            Some("we walked through the Q3 numbers"),
            Some("budget approval lands next week"),
        ]);
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let voice = CapturingVoice::default();
        let spoken = voice.spoken.clone();
        let mut out = CapturingSink::default();

        run_call_session(
            session(false),
            VecFrames::new(frames),
            stt,
            CannedRunner::new("unused answer", seen.clone())
                .with_summary("The team reviewed Q3 numbers; budget approval is due next week."),
            voice,
            &mut out,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        )
        .await;

        // Exactly one runner call: the summary turn (no wake, no task lane turn).
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "only the summary turn runs; got {seen:?}");
        // It carried the summarize instruction and the joined raw transcript.
        assert!(seen[0].starts_with(SUMMARY_INSTRUCTION));
        assert!(
            seen[0].contains("we walked through the Q3 numbers")
                && seen[0].contains("budget approval lands next week"),
            "summary turn must receive the joined transcript; got {:?}",
            seen[0]
        );
        // A summary is never spoken aloud.
        assert!(spoken.lock().unwrap().is_empty(), "summary must not be spoken");
        // The emitted CallSummaryUpdated carries the LLM summary, not the raw join.
        let summary = out
            .events
            .iter()
            .find_map(|e| match e {
                OceanEvent::CallSummaryUpdated { summary, .. } => Some(summary.clone()),
                _ => None,
            })
            .expect("a CallSummaryUpdated must be emitted");
        assert_eq!(
            summary,
            "The team reviewed Q3 numbers; budget approval is due next week."
        );
        assert!(
            !summary.contains("we walked through the Q3 numbers"),
            "must be the LLM output, not the raw transcript join"
        );
    }

    #[tokio::test]
    async fn summary_turn_failure_keeps_prior_summary_and_call_survives() {
        // A runner that errors on the summary turn must not emit a summary and must
        // not drop the call — the prior (here: none) summary simply stands.
        struct FailingSummaryRunner;
        #[async_trait]
        impl TurnRunner for FailingSummaryRunner {
            async fn run(&mut self, command: &str) -> anyhow::Result<String> {
                if command.starts_with(SUMMARY_INSTRUCTION) {
                    anyhow::bail!("summary provider down");
                }
                Ok("ok".into())
            }
        }

        let frames = vec![frame_3s(), frame_3s()];
        let stt = ScriptedStt::new(vec![Some("first thing"), Some("second thing")]);
        let mut out = CapturingSink::default();

        run_call_session(
            session(false),
            VecFrames::new(frames),
            stt,
            FailingSummaryRunner,
            CapturingVoice::default(),
            &mut out,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        )
        .await;

        let types = ev_types(&out);
        // The call still brackets cleanly and transcripts still flow...
        assert_eq!(types.first(), Some(&"started"));
        assert_eq!(types.last(), Some(&"ended"));
        assert!(types.contains(&"segment"), "got {types:?}");
        // ...but a failed summary turn emits NO summary (never the raw transcript).
        assert!(
            !types.contains(&"summary"),
            "failed summary turn must not emit a raw-transcript fallback; got {types:?}"
        );
    }

    #[test]
    fn buffered_ms_matches_sample_math() {
        // 48_000 samples @ 16kHz mono = 3000ms.
        assert_eq!(buffered_ms(48_000, 16_000, 1), 3_000);
        // Stereo: interleaved count is per-channel-doubled.
        assert_eq!(buffered_ms(96_000, 16_000, 2), 3_000);
        assert_eq!(buffered_ms(0, 16_000, 1), 0);
        assert_eq!(buffered_ms(1_000, 0, 1), 0);
    }
}
