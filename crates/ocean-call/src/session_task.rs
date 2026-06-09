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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::Notify;

use crate::frame::PcmFrame;
use crate::orchestrator::{ActiveOutcome, CallSession, EventSink};
use crate::stt::{SegmentUpdate, SttProvider, StreamEvent, TranscriptSegment};
use crate::stt_deepgram::SpeechActivity;
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

// ===========================================================================
// Streaming STT path (OCEAN-242)
//
// The batch loop above buffers an utterance then POSTs a WAV and only learns
// the transcript *after* the speaker pauses. A streaming provider
// ([`SttProvider`], e.g. Deepgram) instead keeps a live socket open: frames are
// pushed in as they arrive and classified [`StreamEvent`]s flow back out in
// real time — interim hypotheses for liveness, finals as the transcript of
// record, and a [`SpeechActivity::Onset`] edge the instant the human starts
// talking. That onset is the **barge-in** signal (OCEAN-243): exposed here so a
// follow-up can cut Ocean's TTS, but not consumed yet.
//
// Rather than bend the batch xAI adapter into the push/stream-out shape (which
// would force the loop's clean size/gap flush policy down into the adapter and
// can't emit interims at all), the streaming path is a *sibling* loop. The batch
// loop is unchanged — every existing test passes verbatim — and the daemon picks
// which to spawn by which provider is configured.
// ===========================================================================

/// Where the loop routes [`SpeechActivity`] edges derived from the live STT
/// stream — the **barge-in seam** lifted out of the session loop.
///
/// OCEAN-242 makes the onset *reachable*; OCEAN-243 wires a real consumer that
/// stops Ocean's in-flight TTS the moment `Onset` fires. Until then the daemon
/// passes [`NoopActivitySink`] and the edge is simply observable (and asserted
/// in tests). Kept as a trait, not a channel, so the consumer can be swapped
/// (log, TTS-canceller, metrics) without touching the loop.
pub trait ActivitySink: Send {
    /// Called for each speech-activity edge the stream produced. `Onset` means
    /// the human just started talking (cut TTS); `Settled` means they paused.
    fn on_activity(&mut self, activity: SpeechActivity);
}

/// Forward through a `&mut` so a caller can lend an [`ActivitySink`] to the loop
/// without giving up ownership (mirrors the blanket impl for [`EventSink`]).
impl<T: ActivitySink + ?Sized> ActivitySink for &mut T {
    fn on_activity(&mut self, activity: SpeechActivity) {
        (**self).on_activity(activity)
    }
}

/// The default barge-in consumer: drops every edge. Used by the daemon until
/// OCEAN-243 lands a real TTS-stop, and by tests that don't assert on activity.
/// The streaming loop still *derives* and forwards edges to it, so swapping in a
/// real sink later is a one-line change at the call site, not a loop change.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopActivitySink;

impl ActivitySink for NoopActivitySink {
    fn on_activity(&mut self, _activity: SpeechActivity) {}
}

/// A capturing [`ActivitySink`] that records every edge — handy for asserting
/// the barge-in onset reached the loop boundary. Shared (`Arc<Mutex<_>>`) so a
/// test can hold a handle while the loop owns the sink.
#[derive(Debug, Clone, Default)]
pub struct CapturingActivitySink {
    /// Every [`SpeechActivity`] edge the loop forwarded, in order.
    pub edges: Arc<std::sync::Mutex<Vec<SpeechActivity>>>,
}

impl ActivitySink for CapturingActivitySink {
    fn on_activity(&mut self, activity: SpeechActivity) {
        self.edges.lock().expect("activity edges lock").push(activity);
    }
}

// ---------------------------------------------------------------------------
// Barge-in (OCEAN-243): human speech cancels Ocean's in-flight TTS.
//
// OCEAN-242 made the [`SpeechActivity::Onset`] edge reachable at the loop
// boundary via [`ActivitySink`], but routed it to [`NoopActivitySink`]. This is
// the real consumer: a shared [`BargeInSignal`] held by BOTH a [`BargeInCanceller`]
// (the active-lane `ActivitySink`) and a [`BargeInVoice`] (a `Voice` decorator).
//
// - The canceller `trigger()`s the signal on `Onset` (human started talking) and
//   `rearm()`s it on `Settled` (they paused), so the *next* answer can speak.
// - `BargeInVoice::speak` races the inner TTS playback against the signal: the
//   instant it fires, the in-flight `speak` is dropped (cancelled) and returns.
//
// The streaming loop forwards every edge to the canceller *as events arrive*,
// and the active-lane answer is run concurrently with continued event draining
// (see `run_answer_barge_in` / `handle_stream_event`), so an `Onset` that lands
// while Ocean is mid-utterance trips the signal and cuts the speak immediately —
// the whole point of OCEAN-243. The `select!` loop body itself is unchanged; the
// call site just swaps `NoopActivitySink` for `BargeInCanceller` and wraps the
// voice in `BargeInVoice` (the "one line" #173 promised).
// ---------------------------------------------------------------------------

/// A one-shot-style cancellation flag shared between the barge-in [`ActivitySink`]
/// and the active-lane [`Voice`]. Cheap to clone (`Arc`); `trigger` is edge-y
/// (idempotent within a spurt) and `rearm` resets it for the next utterance.
///
/// Implemented as an [`AtomicBool`] (so a late speak can see "already barged" the
/// moment it starts) plus a [`Notify`] (so an in-flight speak parked in `await`
/// wakes the instant the human speaks, not on the next polled frame).
#[derive(Clone, Default)]
pub struct BargeInSignal {
    inner: Arc<BargeInInner>,
}

#[derive(Default)]
struct BargeInInner {
    barged: AtomicBool,
    notify: Notify,
}

impl BargeInSignal {
    /// A fresh, un-triggered signal.
    pub fn new() -> Self {
        Self::default()
    }

    /// Trip the signal: the human started talking — cut Ocean's TTS now. Wakes any
    /// `speak` currently parked on [`barged`](Self::barged). Idempotent: repeated
    /// onsets within one talk-spurt are harmless.
    pub fn trigger(&self) {
        self.inner.barged.store(true, Ordering::SeqCst);
        // Wake every waiter (there is at most one in-flight speak, but be safe).
        self.inner.notify.notify_waiters();
    }

    /// Clear the signal so the next answer can speak. Called when the spurt settles
    /// (the human paused) and at the start of each `speak` so a stale onset from a
    /// previous utterance can't pre-cancel a fresh reply.
    pub fn rearm(&self) {
        self.inner.barged.store(false, Ordering::SeqCst);
    }

    /// Whether the signal is currently tripped (a barge-in is in effect).
    pub fn is_barged(&self) -> bool {
        self.inner.barged.load(Ordering::SeqCst)
    }

    /// Resolve as soon as the signal is (or becomes) tripped. Used as the cancel
    /// arm of `BargeInVoice::speak`'s race. Returns immediately if already barged;
    /// otherwise parks until [`trigger`](Self::trigger) fires — no polling.
    pub async fn barged(&self) {
        // Register for notification *before* the load so a `trigger` that races in
        // between can't be missed (notify_waiters only wakes current waiters).
        let notified = self.inner.notify.notified();
        if self.inner.barged.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }
}

/// The real barge-in [`ActivitySink`]: routes [`SpeechActivity`] edges to a shared
/// [`BargeInSignal`]. Swapped in for [`NoopActivitySink`] at the call site to make
/// `Onset` actually stop Ocean's TTS (OCEAN-243).
///
/// - `Onset`  → `trigger()`: the human started talking → cut the in-flight speak.
/// - `Settled`→ `rearm()`: the spurt closed → allow the next answer to speak.
#[derive(Clone, Default)]
pub struct BargeInCanceller {
    signal: BargeInSignal,
}

impl BargeInCanceller {
    /// Build a canceller plus the [`BargeInSignal`] to hand the [`BargeInVoice`] so
    /// the two share one flag. Both are cheap clones of the same `Arc`.
    pub fn new() -> (Self, BargeInSignal) {
        let signal = BargeInSignal::new();
        (
            Self {
                signal: signal.clone(),
            },
            signal,
        )
    }
}

impl ActivitySink for BargeInCanceller {
    fn on_activity(&mut self, activity: SpeechActivity) {
        match activity {
            SpeechActivity::Onset => self.signal.trigger(),
            SpeechActivity::Settled => self.signal.rearm(),
        }
    }
}

/// A [`Voice`] decorator that makes the inner TTS playback cancellable by a
/// [`BargeInSignal`]. `speak` races the inner `speak` against the signal: if the
/// human starts talking mid-utterance the inner future is dropped (cancelled) and
/// `speak` returns `Ok(())` — Ocean stops talking. Wrap the active-lane voice in
/// this and share its signal with a [`BargeInCanceller`].
///
/// Cancellation here means *dropping the inner `speak` future*: for the live
/// [`live::LiveKitVoice`] that stops the `capture_frame` push loop in
/// [`crate::speaker`] between 10ms frames, so no further audio is published — the
/// in-flight utterance is cut, not played to completion.
pub struct BargeInVoice<V> {
    inner: V,
    signal: BargeInSignal,
}

impl<V> BargeInVoice<V> {
    /// Wrap `inner`, cancelling its `speak` whenever `signal` fires. Pass the same
    /// `signal` the [`BargeInCanceller`] holds (from [`BargeInCanceller::new`]).
    pub fn new(inner: V, signal: BargeInSignal) -> Self {
        Self { inner, signal }
    }
}

#[async_trait]
impl<V: Voice> Voice for BargeInVoice<V> {
    async fn speak(&mut self, text: &str) -> anyhow::Result<()> {
        // Clear any stale onset from a prior spurt so a fresh reply isn't
        // pre-cancelled before it utters a single frame.
        self.signal.rearm();
        let signal = self.signal.clone();
        tokio::select! {
            // Bias toward the cancel arm: if the human is already talking when this
            // answer starts, cut it before publishing audio.
            biased;
            _ = signal.barged() => {
                // Human spoke (or is speaking) — abandon the inner speak. Dropping
                // the future here stops the live capture-frame loop mid-utterance.
                tracing::info!("barge-in: human speech cancelled Ocean's TTS mid-utterance");
                Ok(())
            }
            res = self.inner.speak(text) => res,
        }
    }
}

/// The streaming counterpart to [`run_call_session`]: drives a live call through
/// a push/stream-out [`SttProvider`] instead of buffer-then-POST batch STT.
///
/// `stt` is the provider's push side (frames in); `stt_events` is the receiver
/// of classified [`StreamEvent`]s its read pump emits (interim/final segments +
/// barge-in edges). For the live Deepgram provider both come from
/// `DeepgramStt::connect(...) -> (provider, events_rx)`; a test supplies a mock
/// provider whose `push_frame` drives a scripted `events_rx`.
///
/// Contract (matches the batch loop where it overlaps):
/// - emits `CallStarted` up front and `CallEnded` (measured) once `source` ends,
///   so no phantom in-progress call can linger;
/// - pushes every [`PcmFrame`] into `stt` as it arrives (a push error is logged
///   and the frame dropped — a transient socket hiccup never drops the call);
/// - consumes `stt_events`: **final** segments go through
///   [`CallSession::on_segment`] exactly as batch finals do (driving the
///   transcript, task, summary, and wake/answer lanes); **interim** segments are
///   emitted as live `CallTranscriptSegment{is_final:false}` for the surface but
///   never commit to the summary/task lanes;
/// - forwards every [`SpeechActivity`] edge to `activity` — the barge-in onset is
///   thereby reachable for OCEAN-243 (not consumed here);
/// - on source end, calls `stt.finish()` to flush the trailing final, drains the
///   remaining `stt_events`, then closes the call.
#[allow(clippy::too_many_arguments)]
pub async fn run_call_session_streaming<S, R, V, K, A>(
    mut session: CallSession,
    mut source: S,
    stt: Arc<dyn SttProvider>,
    mut stt_events: UnboundedReceiver<StreamEvent>,
    mut runner: R,
    mut voice: V,
    mut sink: K,
    mut activity: A,
    room_id: String,
    participants: Vec<String>,
    policy: UtterancePolicy,
    clock: impl Fn() -> u64 + Send,
) where
    S: FrameSource,
    R: TurnRunner,
    V: Voice,
    K: EventSink,
    A: ActivitySink,
{
    let started_ms = clock();
    session.start(&mut sink, &room_id, participants);

    let mut tick = tokio::time::interval(Duration::from_millis(policy.tick_interval_ms.max(1)));
    // Consume the immediate first tick so it can't pre-empt the first frame.
    tick.tick().await;

    // Tracks whether the provider's read pump is still alive. Once its channel
    // closes, we stop selecting on it — a closed `recv()` resolves to `None`
    // instantly, so with `biased` it would spin hot and starve the audio arm.
    let mut stream_open = true;

    // Phase 1 — live: multiplex the audio source, the STT event stream, and the
    // summary tick until the audio source ends.
    let end = loop {
        tokio::select! {
            // Bias toward draining transcript events first so a backlog of finals
            // is committed before a tick fires a (likely premature) summary.
            biased;

            // Only poll the transcript stream while it's open (see `stream_open`).
            maybe_event = stt_events.recv(), if stream_open => {
                match maybe_event {
                    Some(event) => {
                        handle_stream_event(
                            event,
                            &mut stt_events,
                            &mut session,
                            &mut runner,
                            &mut voice,
                            &mut sink,
                            &mut activity,
                            started_ms,
                            &clock,
                        )
                        .await;
                    }
                    // The provider's read pump ended (socket closed) before the
                    // audio source did. Latch it closed so this arm is no longer
                    // selected; the audio `None` below still drives termination.
                    None => stream_open = false,
                }
            }

            maybe_frame = source.next_frame() => {
                match maybe_frame {
                    Some(frame) => {
                        // Push the frame into the live provider. A push failure is
                        // a transient transport error: log + drop the frame, never
                        // the call (mirrors the batch loop dropping one utterance).
                        if let Err(e) = stt.push_frame(frame).await {
                            tracing::warn!(error = %e, provider = stt.name(), "streaming STT push_frame failed; dropping frame");
                        }
                    }
                    None => break source.lifecycle(), // audio ended — call is over
                }
            }

            _ = tick.tick() => {
                let now = clock().saturating_sub(started_ms);
                if let Some(transcript) = session.on_tick(now) {
                    run_summary(transcript, &mut runner, &mut sink, now).await;
                }
            }
        }
    };

    // Phase 2 — flush: tell the provider the audio ended so it emits the trailing
    // final, then drain whatever the read pump still hands back so the last words
    // (and their lanes) aren't lost. A `finish` error is logged, not fatal.
    if let Err(e) = stt.finish().await {
        tracing::warn!(error = %e, provider = stt.name(), "streaming STT finish failed; draining anyway");
    }
    drain_stream_events(
        &mut stt_events,
        &mut session,
        &mut runner,
        &mut voice,
        &mut sink,
        &mut activity,
        started_ms,
        &clock,
    )
    .await;

    match &end {
        SourceEnd::Ended => tracing::info!(%room_id, "call tap ended cleanly"),
        SourceEnd::Dropped { reason } => {
            tracing::warn!(%room_id, %reason, "call tap dropped mid-call");
        }
    }
    let duration_ms = clock().saturating_sub(started_ms);
    session.end(&mut sink, duration_ms);
}

/// How long Phase 2 waits for the provider's trailing final (flushed in
/// response to `finish()`/CloseStream) before giving up. The trailing utterance
/// is a sub-second event after the socket flush; this is the *upper bound* so a
/// provider that never closes its event sender can't hang the call's teardown.
const TRAILING_FLUSH_GRACE_MS: u64 = 2_000;

/// Drain the STT event stream after `finish()`: first everything already
/// buffered (non-blocking), then — bounded by [`TRAILING_FLUSH_GRACE_MS`] — the
/// provider's trailing final once it arrives.
///
/// Why bounded rather than `while let Some = recv().await`: the session loop
/// holds the provider for its whole lifetime, so a provider that keeps its event
/// sender alive (e.g. a mock, or a real one that doesn't drop the channel on
/// close) would make a blind `recv().await` block forever after the last event.
/// The real Deepgram pump *does* drop its sender when the socket closes, so this
/// loop exits the instant the channel disconnects; the deadline only guards the
/// degenerate case so call teardown is always finite.
#[allow(clippy::too_many_arguments)]
async fn drain_stream_events<R, V, K, A>(
    stt_events: &mut UnboundedReceiver<StreamEvent>,
    session: &mut CallSession,
    runner: &mut R,
    voice: &mut V,
    sink: &mut K,
    activity: &mut A,
    started_ms: u64,
    clock: &(impl Fn() -> u64 + Send),
) where
    R: TurnRunner,
    V: Voice,
    K: EventSink,
    A: ActivitySink,
{
    // (a) Everything already queued — process synchronously, no waiting.
    loop {
        match stt_events.try_recv() {
            Ok(event) => {
                handle_stream_event(
                    event, stt_events, session, runner, voice, sink, activity, started_ms, clock,
                )
                .await;
            }
            // Nothing buffered right now, but the stream is still open — fall
            // through to the bounded wait for the trailing final.
            Err(TryRecvError::Empty) => break,
            // Sender dropped (real provider closed the socket) — fully drained.
            Err(TryRecvError::Disconnected) => return,
        }
    }

    // (b) Bounded wait for the trailing final the provider flushes on finish().
    let deadline = tokio::time::Instant::now() + Duration::from_millis(TRAILING_FLUSH_GRACE_MS);
    loop {
        match tokio::time::timeout_at(deadline, stt_events.recv()).await {
            // A trailing event arrived in time — process it and keep draining
            // (more may follow within the same grace window).
            Ok(Some(event)) => {
                handle_stream_event(
                    event, stt_events, session, runner, voice, sink, activity, started_ms, clock,
                )
                .await;
            }
            // Channel disconnected — provider finished cleanly, we're done.
            Ok(None) => return,
            // Grace elapsed — stop waiting so teardown stays finite.
            Err(_) => {
                tracing::debug!("streaming STT trailing-flush grace elapsed; closing call");
                return;
            }
        }
    }
}

/// Route one classified [`StreamEvent`] from the live STT stream:
/// - forward any [`SpeechActivity`] edge to the barge-in sink — for the real
///   [`BargeInCanceller`] this trips/rearms the TTS-cancel signal (OCEAN-243);
/// - a **final** segment runs the full orchestrator path (transcript + task +
///   summary + wake/answer lanes), exactly as a batch final does;
/// - an **interim** segment is emitted as a live, non-final
///   `CallTranscriptSegment` for the surface — never committed to summary/task.
///
/// `stt_events` is borrowed so the wake-answer lane can keep draining the stream
/// **while Ocean is speaking** (see [`run_answer_barge_in`]): a barge-in `Onset`
/// arrives as its *own* later event, so the answer must concurrently pump the
/// channel to observe it and cut the TTS. (The `select!` loop holds the receiver
/// between events; while parked in an answer here it would otherwise never see
/// the onset.)
#[allow(clippy::too_many_arguments)]
async fn handle_stream_event<R, V, K, A>(
    event: StreamEvent,
    stt_events: &mut UnboundedReceiver<StreamEvent>,
    session: &mut CallSession,
    runner: &mut R,
    voice: &mut V,
    sink: &mut K,
    activity: &mut A,
    started_ms: u64,
    clock: &(impl Fn() -> u64 + Send),
) where
    R: TurnRunner,
    V: Voice,
    K: EventSink,
    A: ActivitySink,
{
    // Barge-in seam first: forward the edge even if the segment itself is an
    // ignored duplicate, so an Onset is never swallowed (the provider already
    // derives the edge from the raw, pre-dedup segment).
    if let Some(edge) = event.activity {
        activity.on_activity(edge);
    }

    match event.update {
        SegmentUpdate::Final(seg) => {
            // A settled final is the transcript of record — run the same lanes a
            // batch transcript would, including the wake answer and the summary
            // turn the orchestrator decides is due.
            let now = clock().saturating_sub(started_ms);
            let outcome = session.on_segment(seg, now, sink);
            if let Some(transcript) = outcome.summary_due {
                run_summary(transcript, runner, sink, now).await;
            }
            if let ActiveOutcome::Answer(command) = outcome.active {
                // Run the answer concurrently with continued event draining so a
                // mid-utterance Onset reaches the canceller and cuts the TTS.
                run_answer_barge_in(
                    command, stt_events, session, runner, voice, sink, activity, started_ms,
                    clock,
                )
                .await;
            }
        }
        SegmentUpdate::Interim(seg) => {
            // Liveness only: surface the interim as a non-final transcript line.
            // It must NOT feed the summary/task/wake lanes (those act on finals),
            // so we emit the event directly rather than going through on_segment.
            sink.emit(ocean_core::OceanEvent::CallTranscriptSegment {
                speaker: seg.speaker,
                text: seg.text,
                start_ms: seg.start_ms,
                is_final: false,
            });
        }
        SegmentUpdate::Ignore => {}
    }
}

/// Run a wake answer on the **streaming** path while keeping the barge-in seam
/// live: the agent turn → speak → `CallAgentSpoke` → cooldown runs concurrently
/// with a pump that keeps reading `stt_events` and forwarding each event's
/// [`SpeechActivity`] edge to `activity`. With the real [`BargeInCanceller`] +
/// [`BargeInVoice`], an `Onset` that lands while Ocean is speaking trips the
/// shared signal, which cancels the in-flight `voice.speak`, so Ocean stops
/// talking the instant the human does.
///
/// Why a dedicated pump rather than running [`run_answer`] bare: the main
/// `select!` loop owns `stt_events` and is *parked here* for the whole answer, so
/// without this it would never dequeue the onset event until speak already
/// finished — exactly the OCEAN-243 bug. The interim/final *segments* that arrive
/// during the answer are intentionally **not** re-fed to the orchestrator (they
/// are the human's barge-in speech; re-entrant `on_segment` mid-answer would be
/// wrong) — they're rendered as live transcript lines and their barge-in edge is
/// honoured, which is all the cancel decision needs. Once the answer completes
/// (or is cancelled), the main loop resumes normal full-fat draining.
///
/// State consistency: `run_answer` is unchanged and still calls
/// [`CallSession::mark_replied`] at the end — whether the speak finished or was
/// cut short — so the wake echo-cooldown always starts and the session never
/// thinks it is still speaking after a barge-in.
///
/// No lost transcripts: while the answer holds `session`/`sink`/`runner`/`voice`,
/// the concurrent pump can't run them, so any event it pulls off `stt_events` is
/// **stashed** (its edge forwarded live for the cancel decision) and then, once
/// the answer resolves and those borrows free, **replayed in order** through the
/// normal segment path. So a `Final` that happens to arrive during the speak
/// window is still committed — never dropped — and a single writer for `sink` is
/// preserved throughout.
#[allow(clippy::too_many_arguments)]
async fn run_answer_barge_in<R, V, K, A>(
    command: String,
    stt_events: &mut UnboundedReceiver<StreamEvent>,
    session: &mut CallSession,
    runner: &mut R,
    voice: &mut V,
    sink: &mut K,
    activity: &mut A,
    started_ms: u64,
    clock: &(impl Fn() -> u64 + Send),
) where
    R: TurnRunner,
    V: Voice,
    K: EventSink,
    A: ActivitySink,
{
    // Events the pump consumed while the answer was in flight, to replay (segment
    // side only) once the answer resolves and the borrows are free.
    let mut pending: Vec<StreamEvent> = Vec::new();

    {
        // The answer future (turn → speak → spoke → cooldown). With a `BargeInVoice`
        // the `speak` inside races the shared signal, so this resolves early if the
        // pump below trips it. Scoped so its borrows of session/sink/runner/voice
        // end before we replay `pending`.
        let answer = run_answer(command, session, runner, voice, sink, started_ms, clock);
        tokio::pin!(answer);

        loop {
            tokio::select! {
                // Bias toward the answer so it makes progress / completes promptly;
                // the pump only needs to win when the answer is parked in `speak`.
                biased;

                // The answer finished (spoke fully, was cancelled by barge-in, or
                // failed) — stop pumping; replay anything we stashed below.
                _ = &mut answer => break,

                // Keep the barge-in seam alive while Ocean speaks: forward each
                // arriving event's activity edge to the canceller. An `Onset` here
                // trips the signal the `BargeInVoice` is racing → speak cancels.
                // Stash the whole event so its segment isn't lost (replayed below).
                maybe_event = stt_events.recv() => {
                    match maybe_event {
                        Some(event) => {
                            if let Some(edge) = event.activity {
                                activity.on_activity(edge);
                            }
                            pending.push(event);
                        }
                        // Stream closed mid-answer: stop pumping but let the answer
                        // run to completion first (its arm wins the next turn).
                        None => {
                            (&mut answer).await;
                            break;
                        }
                    }
                }
            }
        }
    }

    // Answer is done; borrows are free. Replay the stashed events' **segment side**
    // (the edges were already forwarded live above, so don't re-forward them) so a
    // `Final` that arrived during the speak window is still committed in order.
    for event in pending {
        handle_segment_update(event.update, session, runner, voice, sink, started_ms, clock).await;
    }
}

/// Apply just the [`SegmentUpdate`] side of a stream event (no activity edge): a
/// `Final` runs the full orchestrator path (transcript + task + summary + wake/
/// answer), an `Interim` renders a live non-final line, `Ignore` is dropped.
///
/// Factored out of [`handle_stream_event`] so [`run_answer_barge_in`] can replay
/// events it stashed during a barge-in **without** re-forwarding their (already
/// handled) activity edge. The wake/answer here uses the plain [`run_answer`] —
/// a replayed final answered after the barge-in doesn't itself need the
/// concurrent pump (the human already settled), and nesting the pump would
/// needlessly re-borrow `stt_events`.
async fn handle_segment_update<R, V, K>(
    update: SegmentUpdate,
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
    match update {
        SegmentUpdate::Final(seg) => {
            let now = clock().saturating_sub(started_ms);
            let outcome = session.on_segment(seg, now, sink);
            if let Some(transcript) = outcome.summary_due {
                run_summary(transcript, runner, sink, now).await;
            }
            if let ActiveOutcome::Answer(command) = outcome.active {
                run_answer(command, session, runner, voice, sink, started_ms, clock).await;
            }
        }
        SegmentUpdate::Interim(seg) => {
            sink.emit(ocean_core::OceanEvent::CallTranscriptSegment {
                speaker: seg.speaker,
                text: seg.text,
                start_ms: seg.start_ms,
                is_final: false,
            });
        }
        SegmentUpdate::Ignore => {}
    }
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
            // Bound the batch STT call. Without these, a stalled xAI response
            // leaves `.send().await` (and `.json().await`) pending forever,
            // which blocks the orchestrator's per-segment transcribe loop for
            // the entire call. These are non-streaming POSTs (WAV in, JSON
            // out), so a full request timeout is correct and safe here:
            //   connect 10s — establish the TLS connection;
            //   request 60s — the whole batch transcription, generous enough
            //   for a long utterance but bounded so a hang can't freeze the call.
            let client = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());
            Self {
                client,
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

    /// Turns reply text into TTS PCM to push into the call. The verified provider
    /// is [`crate::tts_xai`] (xAI TTS → WAV → 16kHz mono PCM), wired in via
    /// [`default_tts_synth`] when the `xai-tts` feature is on; swap in any other
    /// synthesizer (ElevenLabs/Cartesia → 16kHz mono PCM) behind this trait and
    /// the [`LiveKitVoice`] below speaks it, no other change.
    #[async_trait]
    pub trait TtsSynth: Send {
        /// Synthesize `text` to a single 16kHz-mono PCM utterance.
        async fn synth(&mut self, text: &str) -> anyhow::Result<PcmFrame>;
    }

    /// Lets a boxed synth satisfy the trait, so [`default_tts_synth`] can return
    /// either the live xAI synth or the silence fallback as one `Box<dyn TtsSynth>`
    /// without the caller naming the concrete type.
    #[async_trait]
    impl TtsSynth for Box<dyn TtsSynth> {
        async fn synth(&mut self, text: &str) -> anyhow::Result<PcmFrame> {
            (**self).synth(text).await
        }
    }

    /// The no-provider fallback: logs the reply and yields a short silence so the
    /// publish path is still exercised end-to-end (and `CallAgentSpoke` still
    /// fires) without fabricating speech. Used when the `xai-tts` feature is off.
    pub struct SilenceTts;

    #[async_trait]
    impl TtsSynth for SilenceTts {
        async fn synth(&mut self, text: &str) -> anyhow::Result<PcmFrame> {
            tracing::warn!(
                reply = %text,
                "call TTS synth not wired (xai-tts feature off) — emitting silence; \
                 CallAgentSpoke still fires"
            );
            // 200ms of 16kHz mono silence as a benign placeholder utterance.
            Ok(PcmFrame::new(vec![0i16; 3_200], 16_000, 1))
        }
    }

    /// Pick the active-lane TTS synth: the live xAI synth when the `xai-tts`
    /// feature is compiled in (using `api_key`), otherwise [`SilenceTts`]. Boxed
    /// so both arms share one return type, letting the daemon wire TTS without a
    /// `cfg` at the call site — the default build stays silent and credential-free.
    pub fn default_tts_synth(api_key: &str) -> Box<dyn TtsSynth> {
        let _ = api_key;
        #[cfg(feature = "xai-tts")]
        let synth: Box<dyn TtsSynth> =
            Box::new(crate::tts_xai::live::XaiTtsSynth::new(api_key.to_string()));
        #[cfg(not(feature = "xai-tts"))]
        let synth: Box<dyn TtsSynth> = Box::new(SilenceTts);
        synth
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
            // Lazily publish the voice track on first use. Structured so the type
            // proves the source exists by the time we borrow it — no unwrap/expect
            // on the live call path. If publishing fails, `?` propagates the error
            // and the speak attempt fails gracefully (the call keeps running); it
            // never panics the per-call task.
            Ok(match self.source {
                Some(ref src) => src,
                None => {
                    let src = crate::speaker::live::publish_voice_track(
                        &self.room,
                        self.sample_rate,
                        self.num_channels,
                    )
                    .await?;
                    self.source.insert(src)
                }
            })
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

    /// A voice whose TTS transport always fails (synth down / publish dropped),
    /// while still recording what it was *asked* to speak. Models the live
    /// `LiveKitVoice::speak` returning `Err` (synth error or publish failure) so a
    /// test can assert the call survives and `CallAgentSpoke` is still emitted —
    /// the operator rail must show what Ocean said even when the audio leg fails.
    #[derive(Clone, Default)]
    struct FailingVoice {
        attempts: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Voice for FailingVoice {
        async fn speak(&mut self, text: &str) -> anyhow::Result<()> {
            self.attempts.lock().unwrap().push(text.to_string());
            anyhow::bail!("TTS transport down")
        }
    }

    /// An STT that errors on every utterance (provider 5xx / network drop). The
    /// loop must skip the utterance and keep running, never panic or abort the
    /// call. Counts attempts so a test can prove the loop kept feeding it.
    struct ErroringStt {
        attempts: Arc<AtomicU64>,
    }

    #[async_trait]
    impl Transcriber for ErroringStt {
        async fn transcribe(
            &mut self,
            _wav: Vec<u8>,
            _start_ms: u64,
        ) -> anyhow::Result<Option<TranscriptSegment>> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("STT provider unavailable")
        }
    }

    /// An STT that errors on the first N utterances, then yields scripted text —
    /// proving a transient STT failure mid-call is survived and later utterances
    /// still transcribe (resilience, not just "doesn't crash on the first error").
    struct FlakyStt {
        fail_first: u64,
        seen: u64,
        scripts: VecDeque<Option<String>>,
    }

    impl FlakyStt {
        fn new(fail_first: u64, scripts: Vec<Option<&str>>) -> Self {
            Self {
                fail_first,
                seen: 0,
                scripts: scripts.into_iter().map(|s| s.map(String::from)).collect(),
            }
        }
    }

    #[async_trait]
    impl Transcriber for FlakyStt {
        async fn transcribe(
            &mut self,
            _wav: Vec<u8>,
            start_ms: u64,
        ) -> anyhow::Result<Option<TranscriptSegment>> {
            self.seen += 1;
            if self.seen <= self.fail_first {
                anyhow::bail!("STT transient error (attempt {})", self.seen);
            }
            match self.scripts.pop_front() {
                Some(Some(text)) => Ok(Some(TranscriptSegment::final_("caller", text, start_ms))),
                _ => Ok(None),
            }
        }
    }

    fn frame_3s() -> PcmFrame {
        // 16kHz mono, 3000ms = 48_000 samples → flushes on the 3s size policy.
        PcmFrame::new(vec![1i16; 48_000], 16_000, 1)
    }

    fn session(muted: bool) -> CallSession {
        session_cooldown(muted, 2_000)
    }

    /// Like [`session`] but with an explicit wake echo-cooldown. The barge-in
    /// end-to-end test fires two wake answers back-to-back on a fake clock that
    /// barely advances, so it needs a near-zero cooldown to keep the second from
    /// being suppressed as Ocean's own echo (the cooldown is correct, just larger
    /// than the test's compressed timeline).
    fn session_cooldown(muted: bool, cooldown_ms: u64) -> CallSession {
        CallSession::new(
            "call-test",
            Summarizer::new(SummaryPolicy {
                every_n_segments: 2,
                silence_ms: 5_000,
            }),
            WakeGate::new(muted, cooldown_ms),
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

    // -----------------------------------------------------------------------
    // Failure modes (OCEAN-230). The four outside-world seams can all fail in
    // production — STT 5xx, the agent turn timing out, TTS synth/publish
    // dropping. The loop's contract is that NONE of these aborts the call: a
    // failing utterance is dropped, a failing answer/summary is logged and
    // skipped, and the call still brackets cleanly with CallStarted/CallEnded.
    // These tests drive each error arm in `flush_utterance` / `run_answer` /
    // `run_summary` and assert survival, exercising the paths the happy-path
    // tests above can't reach.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn stt_error_mid_call_is_skipped_and_call_survives() {
        // Transcriber errors on the only utterance. The loop must drop it (no
        // segment), keep running, and still close the call cleanly — an STT
        // outage degrades to "no transcript", never a dropped call.
        let frames = vec![frame_3s()];
        let attempts = Arc::new(AtomicU64::new(0));
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let voice = CapturingVoice::default();
        let spoken = voice.spoken.clone();
        let mut out = CapturingSink::default();

        run_call_session(
            session(false),
            VecFrames::new(frames),
            ErroringStt {
                attempts: attempts.clone(),
            },
            CannedRunner::new("unused", seen.clone()),
            voice,
            &mut out,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        )
        .await;

        // The STT was actually driven (the utterance reached transcribe)...
        assert!(attempts.load(Ordering::SeqCst) >= 1, "STT must have been called");
        // ...but the failure produced no transcript and the call still bracketed.
        assert_eq!(
            ev_types(&out),
            vec!["started", "ended"],
            "STT error must drop the utterance, not the call"
        );
        // No downstream lane ran off a failed transcription.
        assert!(seen.lock().unwrap().is_empty(), "no agent turn off failed STT");
        assert!(spoken.lock().unwrap().is_empty(), "nothing spoken off failed STT");
    }

    #[tokio::test]
    async fn stt_recovers_after_transient_error() {
        // First utterance's STT fails; the second succeeds and must transcribe
        // normally. Proves a transient STT error mid-call is survived AND the
        // pipeline keeps working afterward — not just "no panic on first error".
        let frames = vec![frame_3s(), frame_3s()];
        let stt = FlakyStt::new(1, vec![Some("the second utterance came through")]);
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
        assert_eq!(types.first(), Some(&"started"));
        assert_eq!(types.last(), Some(&"ended"));
        // Exactly the recovered utterance produced a segment.
        assert!(
            types.contains(&"segment"),
            "post-error utterance must still transcribe; got {types:?}"
        );
        let seg = out.events.iter().find_map(|e| match e {
            OceanEvent::CallTranscriptSegment { text, .. } => Some(text.clone()),
            _ => None,
        });
        assert_eq!(seg.as_deref(), Some("the second utterance came through"));
    }

    #[tokio::test]
    async fn agent_turn_failure_on_wake_does_not_drop_call() {
        // The wake fires, but the agent turn errors. The call must survive: no
        // CallAgentSpoke, nothing spoken, but CallWakeTriggered already fired and
        // the call still ends cleanly. (mark_replied is still called so a failed
        // answer can't re-trigger on the same echo — covered by survival here.)
        struct FailingAnswerRunner {
            seen: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl TurnRunner for FailingAnswerRunner {
            async fn run(&mut self, command: &str) -> anyhow::Result<String> {
                self.seen.lock().unwrap().push(command.to_string());
                anyhow::bail!("agent runtime unavailable")
            }
        }

        let frames = vec![frame_3s()];
        let stt = ScriptedStt::new(vec![Some("hey Ocean what did we decide")]);
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let voice = CapturingVoice::default();
        let spoken = voice.spoken.clone();
        let mut out = CapturingSink::default();

        run_call_session(
            session(false),
            VecFrames::new(frames),
            stt,
            FailingAnswerRunner { seen: seen.clone() },
            voice,
            &mut out,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        )
        .await;

        let types = ev_types(&out);
        // Wake fired and the runner WAS driven over the command...
        assert!(types.contains(&"wake"), "wake should still trigger; got {types:?}");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &["what did we decide".to_string()],
            "the agent turn was attempted over the wake command"
        );
        // ...but the failed turn spoke nothing and emitted no CallAgentSpoke...
        assert!(!types.contains(&"spoke"), "failed turn must not emit spoke; got {types:?}");
        assert!(spoken.lock().unwrap().is_empty(), "failed turn speaks nothing");
        // ...and the call still closed cleanly (no panic, no abort).
        assert_eq!(types.first(), Some(&"started"));
        assert_eq!(types.last(), Some(&"ended"));
    }

    #[tokio::test]
    async fn tts_failure_still_emits_spoke_and_survives() {
        // The agent turn succeeds but TTS playback fails. Contract (run_answer):
        // CallAgentSpoke is emitted REGARDLESS of TTS transport success, so the
        // operator rail shows what Ocean said even when the audio leg degrades —
        // and a failed speak never drops the call.
        let frames = vec![frame_3s()];
        let stt = ScriptedStt::new(vec![Some("hey Ocean summarize that")]);
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let voice = FailingVoice::default();
        let attempts = voice.attempts.clone();
        let mut out = CapturingSink::default();

        run_call_session(
            session(false),
            VecFrames::new(frames),
            stt,
            CannedRunner::new("Here is the summary you asked for.", seen.clone()),
            voice,
            &mut out,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        )
        .await;

        let types = ev_types(&out);
        // TTS was actually attempted with the reply text...
        assert_eq!(
            attempts.lock().unwrap().as_slice(),
            &["Here is the summary you asked for.".to_string()],
            "voice.speak must be attempted with the agent reply"
        );
        // ...and despite the transport failure, CallAgentSpoke still fired with
        // the reply text (the load-bearing contract for the operator rail)...
        assert!(types.contains(&"spoke"), "spoke must fire even when TTS fails; got {types:?}");
        let spoke_text = out.events.iter().find_map(|e| match e {
            OceanEvent::CallAgentSpoke { text } => Some(text.clone()),
            _ => None,
        });
        assert_eq!(spoke_text.as_deref(), Some("Here is the summary you asked for."));
        // ...and the call survived the TTS failure and closed cleanly.
        assert_eq!(types.first(), Some(&"started"));
        assert_eq!(types.last(), Some(&"ended"));
    }

    #[tokio::test]
    async fn empty_reply_is_not_spoken_and_call_survives() {
        // A wake turn that returns whitespace-only text: run_answer trims to empty
        // and must NOT speak or emit CallAgentSpoke (no empty bubble on the rail),
        // yet still completes the answer (cooldown) and the call ends cleanly.
        let frames = vec![frame_3s()];
        let stt = ScriptedStt::new(vec![Some("hey Ocean anything")]);
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let voice = CapturingVoice::default();
        let spoken = voice.spoken.clone();
        let mut out = CapturingSink::default();

        run_call_session(
            session(false),
            VecFrames::new(frames),
            stt,
            CannedRunner::new("   ", seen.clone()), // whitespace-only reply
            voice,
            &mut out,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        )
        .await;

        let types = ev_types(&out);
        // The turn ran (wake fired, runner saw the command)...
        assert!(types.contains(&"wake"), "wake should fire; got {types:?}");
        assert_eq!(seen.lock().unwrap().len(), 1, "the answer turn ran");
        // ...but an empty reply produces no speech and no spoke event...
        assert!(spoken.lock().unwrap().is_empty(), "empty reply must not be spoken");
        assert!(!types.contains(&"spoke"), "empty reply must not emit spoke; got {types:?}");
        // ...and the call still closes cleanly.
        assert_eq!(types.last(), Some(&"ended"));
    }

    #[tokio::test]
    async fn cascading_failures_still_close_the_call() {
        // Worst case: STT errors on utterance 1, then utterance 2 transcribes a
        // wake command whose agent turn ALSO errors, then the source drops. The
        // call must still bracket and end on the drop — no single failure, and no
        // pile-up of failures, can leave a phantom in-progress call.
        struct AlwaysFailRunner;
        #[async_trait]
        impl TurnRunner for AlwaysFailRunner {
            async fn run(&mut self, _command: &str) -> anyhow::Result<String> {
                anyhow::bail!("runtime down")
            }
        }

        let frames = vec![frame_3s(), frame_3s()];
        let stt = FlakyStt::new(1, vec![Some("hey Ocean what's the status")]);
        let mut out = CapturingSink::default();

        run_call_session(
            session(false),
            VecFrames::dropped(frames, "PeerConnectionFailed"),
            stt,
            AlwaysFailRunner,
            FailingVoice::default(),
            &mut out,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        )
        .await;

        let types = ev_types(&out);
        // Even with STT + agent-turn + TTS all failing and the tap dropping, the
        // call brackets cleanly. CallEnded is non-negotiable.
        assert_eq!(types.first(), Some(&"started"));
        assert_eq!(types.last(), Some(&"ended"));
        assert!(!types.contains(&"spoke"), "no successful speak amid failures; got {types:?}");
    }

    // -----------------------------------------------------------------------
    // OCEAN-241: the live-voice lazy-init-then-borrow invariant.
    //
    // `LiveKitVoice::ensure_source` (in `session_task::live`, behind the
    // `livekit-tap` native feature) publishes its audio source on first use,
    // then borrows it. It used to do that as `self.source = Some(src);
    // Ok(self.source.as_ref().expect("source just set"))` — an `.expect()` on a
    // value the prior line had just set. That expect can't be reached in the
    // happy path, but it's a non-test panic site on the live call task: if the
    // structure ever drifts (an early return, a refactor that clears the field),
    // it panics the per-call task and can poison shared state instead of failing
    // the one speak attempt. The fix removes the expect entirely by returning
    // the value `Option::insert` hands back, so the type — not a runtime check —
    // proves the source exists.
    //
    // `LiveKitVoice` needs a real native `livekit::Room`, so it can't be built
    // here without the feature + a live connection. This test instead pins the
    // exact control-flow shape the fix relies on, with a local stand-in, and
    // asserts: (1) the init runs once on the `None` branch and the borrow yields
    // the freshly-inserted value (the arm that replaced the `expect`), (2) a
    // second call takes the `Some` branch and does NOT re-init, and (3) an init
    // failure propagates via `?` and never panics — the call survives a publish
    // failure as a failed speak, mirroring `ensure_source`'s real contract.
    #[tokio::test]
    async fn ensure_source_invariant_never_panics_on_lazy_init() {
        // A stand-in mirroring `LiveKitVoice`'s lazy source field + ensure path,
        // structured identically to the production `ensure_source` so this guards
        // the same invariant without the native livekit dependency.
        struct LazySource {
            source: Option<String>,
            inits: u32,
            fail: bool,
        }
        impl LazySource {
            // Same shape as the fixed `ensure_source`: match on the Option, and on
            // the `None` arm publish-then-`insert`, returning what `insert` hands
            // back. No `.unwrap()` / `.expect()` anywhere on this path.
            async fn ensure(&mut self) -> anyhow::Result<&str> {
                Ok(match self.source {
                    Some(ref s) => s,
                    None => {
                        self.inits += 1;
                        if self.fail {
                            anyhow::bail!("publish_voice_track failed");
                        }
                        // Stands in for `publish_voice_track(...).await?`.
                        self.source.insert("track-published".to_string())
                    }
                })
            }
        }

        // (1) None branch: init runs once, borrow yields the inserted value.
        let mut v = LazySource { source: None, inits: 0, fail: false };
        let first = v.ensure().await.expect("first ensure must succeed");
        assert_eq!(first, "track-published", "None arm must return the inserted source");
        assert_eq!(v.inits, 1, "init must run exactly once on first use");

        // (2) Some branch: no re-init, same value, still no panic.
        let again = v.ensure().await.expect("second ensure must succeed");
        assert_eq!(again, "track-published");
        assert_eq!(v.inits, 1, "subsequent calls must not re-publish the source");

        // (3) Init failure propagates via `?` as Err — never a panic. A live
        // publish failure fails the one speak attempt; the call keeps running.
        let mut bad = LazySource { source: None, inits: 0, fail: true };
        let err = bad.ensure().await;
        assert!(err.is_err(), "a publish failure must surface as Err, not a panic");
        assert!(bad.source.is_none(), "a failed init must leave the source unset");
    }

    // =======================================================================
    // Streaming STT path (OCEAN-242).
    //
    // `run_call_session_streaming` drives a call through a push/stream-out
    // `SttProvider` instead of buffer-then-POST batch STT. These tests prove,
    // with a mock provider (no socket), that:
    //   - **final** segments off the stream reach `CallSession::on_segment` and
    //     drive the full passive + active lanes (transcript, task, summary,
    //     wake/answer) exactly as batch finals do;
    //   - **interim** segments surface as non-final transcript lines but never
    //     feed the summary/task/wake lanes;
    //   - the `SpeechActivity::Onset` **barge-in** edge is forwarded out of the
    //     loop to the `ActivitySink` (reachable for OCEAN-243), even though no
    //     consumer acts on it yet;
    //   - the call still brackets cleanly with CallStarted/CallEnded, including
    //     when the stream closes early or the source drops.
    // =======================================================================

    /// A mock streaming [`SttProvider`]. On the first `push_frame` it flushes a
    /// scripted batch of [`StreamEvent`]s onto the channel the loop drains; on
    /// `finish()` it flushes a scripted trailing batch (the provider's "trailing
    /// final" on CloseStream) and then **drops its sender** — exactly mirroring
    /// the real Deepgram pump, whose `events_tx` dies when the socket closes
    /// after CloseStream. Dropping the sender disconnects the channel, so the
    /// loop's bounded Phase-2 drain returns the instant it's done (no real-time
    /// grace wait in tests). Because the channel is unbounded and the loop drains
    /// it both live and post-`finish`, every scripted event is processed before
    /// `CallEnded` regardless of select ordering — fully deterministic.
    struct MockStreamingStt {
        /// `Option` so `finish()` can drop the sender (close the stream). Behind a
        /// `std::sync::Mutex` because `SttProvider` is `&self`.
        tx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<StreamEvent>>>,
        /// Emitted on the first `push_frame` (interims + finals during the call).
        on_first_frame: std::sync::Mutex<Option<Vec<StreamEvent>>>,
        /// Emitted on `finish()` (the trailing final flushed on CloseStream).
        on_finish: std::sync::Mutex<Option<Vec<StreamEvent>>>,
        pushes: Arc<AtomicU64>,
        finished: Arc<AtomicU64>,
    }

    impl MockStreamingStt {
        /// Build the provider + the receiver the loop consumes. `during` is sent
        /// on the first frame; `trailing` is sent on `finish()`.
        fn new(
            during: Vec<StreamEvent>,
            trailing: Vec<StreamEvent>,
        ) -> (Arc<Self>, UnboundedReceiver<StreamEvent>, Arc<AtomicU64>, Arc<AtomicU64>) {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let pushes = Arc::new(AtomicU64::new(0));
            let finished = Arc::new(AtomicU64::new(0));
            let me = Arc::new(Self {
                tx: std::sync::Mutex::new(Some(tx)),
                on_first_frame: std::sync::Mutex::new(Some(during)),
                on_finish: std::sync::Mutex::new(Some(trailing)),
                pushes: pushes.clone(),
                finished: finished.clone(),
            });
            (me, rx, pushes, finished)
        }

        /// Send a batch through the (still-open) sender. A closed receiver just
        /// means the loop already ended; ignore the send error.
        fn emit(&self, batch: Vec<StreamEvent>) {
            if let Some(tx) = self.tx.lock().unwrap().as_ref() {
                for ev in batch {
                    let _ = tx.send(ev);
                }
            }
        }
    }

    #[async_trait]
    impl SttProvider for MockStreamingStt {
        async fn push_frame(&self, _frame: PcmFrame) -> anyhow::Result<()> {
            self.pushes.fetch_add(1, Ordering::SeqCst);
            // Flush the scripted in-call events exactly once, on the first frame.
            if let Some(batch) = self.on_first_frame.lock().unwrap().take() {
                self.emit(batch);
            }
            Ok(())
        }

        async fn finish(&self) -> anyhow::Result<()> {
            self.finished.fetch_add(1, Ordering::SeqCst);
            if let Some(batch) = self.on_finish.lock().unwrap().take() {
                self.emit(batch);
            }
            // Drop the sender so the channel disconnects — models the real pump's
            // sender dying when the socket closes after CloseStream.
            *self.tx.lock().unwrap() = None;
            Ok(())
        }

        fn name(&self) -> &'static str {
            "mock-streaming"
        }
    }

    /// Convenience: a `StreamEvent` carrying a final segment, no activity edge.
    fn final_event(text: &str, start_ms: u64) -> StreamEvent {
        StreamEvent {
            update: SegmentUpdate::Final(TranscriptSegment::final_("caller", text, start_ms)),
            activity: None,
        }
    }

    /// Convenience: a `StreamEvent` carrying an interim segment + a barge-in
    /// Onset edge — exactly what the provider emits at the leading edge of a
    /// talk-spurt (first non-empty interim after silence).
    fn interim_onset_event(text: &str, start_ms: u64) -> StreamEvent {
        StreamEvent {
            update: SegmentUpdate::Interim(TranscriptSegment::interim("caller", text, start_ms)),
            activity: Some(SpeechActivity::Onset),
        }
    }

    #[tokio::test]
    async fn streaming_finals_reach_session_and_onset_is_exposed() {
        // The headline OCEAN-242 test. A talk-spurt's leading interim carries the
        // barge-in Onset; a final settles it (carrying Settled); a second final
        // crosses the every_n_segments=2 threshold so the summary lane fires; and
        // a wake command drives the active lane. Asserts: interim surfaced as a
        // non-final segment, BOTH finals reached the orchestrator (transcript +
        // task + summary + wake/answer), and the Onset edge reached the loop's
        // ActivitySink (the barge-in signal exposed for OCEAN-243).
        let during = vec![
            // Leading interim of a spurt → Onset (barge-in seam) + live render.
            interim_onset_event("I'll send the", 0),
            // The spurt settles into a final: a task commitment. Settled edge too.
            StreamEvent {
                update: SegmentUpdate::Final(TranscriptSegment::final_(
                    "caller",
                    "I'll send the master to Atlantic tonight",
                    0,
                )),
                activity: Some(SpeechActivity::Settled),
            },
            // A second final crosses the summary threshold (every_n_segments=2).
            final_event("the release is locked for friday", 1_000),
            // A wake command drives the active lane (answer → speak → spoke).
            final_event("hey Ocean what did we agree to", 2_000),
        ];

        let (stt, rx, pushes, finished) = MockStreamingStt::new(during, vec![]);
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let voice = CapturingVoice::default();
        let spoken = voice.spoken.clone();
        let activity = CapturingActivitySink::default();
        let edges = activity.edges.clone();
        let mut out = CapturingSink::default();

        run_call_session_streaming(
            session(false),
            // One frame is enough to trigger the scripted in-call flush; the
            // source then ends, driving finish() + the trailing drain.
            VecFrames::new(vec![frame_3s()]),
            stt,
            rx,
            CannedRunner::new("You agreed to send the master.", seen.clone())
                .with_summary("Caller will send the master; release locked for Friday."),
            voice,
            &mut out,
            activity,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        )
        .await;

        // The provider was actually driven: frames pushed in, finish() called once.
        assert!(pushes.load(Ordering::SeqCst) >= 1, "frames must be pushed to the provider");
        assert_eq!(finished.load(Ordering::SeqCst), 1, "finish() must be called exactly once on source end");

        let types = ev_types(&out);
        // Lifecycle brackets the call.
        assert_eq!(types.first(), Some(&"started"));
        assert_eq!(types.last(), Some(&"ended"));
        // Finals drove the passive lanes: transcript(s), the detected task, summary.
        assert!(types.contains(&"segment"), "got {types:?}");
        assert!(types.contains(&"task"), "got {types:?}");
        assert!(types.contains(&"summary"), "got {types:?}");
        // Active lane: wake fired off a final, the agent answered, Ocean spoke.
        assert!(types.contains(&"wake"), "got {types:?}");
        assert!(types.contains(&"spoke"), "got {types:?}");

        // The interim surfaced as a NON-final transcript line (liveness), and the
        // task-commitment final surfaced as a FINAL line — proving finals reach
        // CallSession::on_segment (which is what emits the transcript segments).
        let interim_rendered = out.events.iter().any(|e| matches!(
            e,
            OceanEvent::CallTranscriptSegment { text, is_final: false, .. } if text == "I'll send the"
        ));
        assert!(interim_rendered, "interim must surface as a non-final transcript line");
        let final_rendered = out.events.iter().any(|e| matches!(
            e,
            OceanEvent::CallTranscriptSegment { text, is_final: true, .. }
                if text == "I'll send the master to Atlantic tonight"
        ));
        assert!(final_rendered, "the final segment must reach the orchestrator and render as final");

        // The summary turn ran over the JOINED FINALS (not the interim), and the
        // wake command ran — proving finals (only) fed the summary + wake lanes.
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "runner drives summary + answer; got {seen:?}");
        assert!(seen[0].starts_with(SUMMARY_INSTRUCTION), "first turn is the summary turn");
        assert!(
            seen[0].contains("I'll send the master to Atlantic tonight")
                && seen[0].contains("the release is locked for friday"),
            "summary turn must carry the joined FINAL transcript; got {:?}",
            seen[0]
        );
        // The summarizer joins FINALS only: exactly the two final texts, in order,
        // with nothing from the interim committed as its own line. (The interim
        // string "I'll send the" is necessarily a prefix-substring of the first
        // final, so we assert on the committed shape, not substring absence.)
        assert_eq!(
            seen[0],
            format!(
                "{SUMMARY_INSTRUCTION}\n\nTranscript:\nI'll send the master to Atlantic tonight \
                 the release is locked for friday"
            ),
            "summary transcript must be exactly the two joined finals — no interim line",
        );
        assert_eq!(seen[1], "what did we agree to", "second turn is the wake command, stripped");
        assert_eq!(
            spoken.lock().unwrap().as_slice(),
            &["You agreed to send the master.".to_string()],
            "the wake answer is spoken; the summary is not"
        );

        // The load-bearing OCEAN-242 assertion: the barge-in Onset edge reached
        // the loop boundary (the ActivitySink), so OCEAN-243 can consume it. The
        // Settled edge that closed the spurt is forwarded too.
        let edges = edges.lock().unwrap().clone();
        assert!(
            edges.contains(&SpeechActivity::Onset),
            "the barge-in Onset edge must be exposed to the ActivitySink; got {edges:?}"
        );
        assert_eq!(
            edges.first(),
            Some(&SpeechActivity::Onset),
            "Onset is the leading edge of the spurt; got {edges:?}"
        );
        assert!(
            edges.contains(&SpeechActivity::Settled),
            "the Settled edge that closed the spurt should also be forwarded; got {edges:?}"
        );
    }

    #[tokio::test]
    async fn streaming_trailing_final_on_finish_is_drained() {
        // The provider holds back its last final until CloseStream (`finish()`) —
        // exactly how Deepgram flushes the trailing utterance. The loop must drain
        // that post-finish event so the last words still reach the orchestrator
        // and aren't lost when the audio source ends.
        let (stt, rx, _pushes, finished) =
            MockStreamingStt::new(vec![], vec![final_event("just a closing thought", 500)]);
        let mut out = CapturingSink::default();

        run_call_session_streaming(
            session(false),
            VecFrames::new(vec![frame_3s()]),
            stt,
            rx,
            CannedRunner::new("x", Default::default()),
            CapturingVoice::default(),
            &mut out,
            NoopActivitySink,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        )
        .await;

        assert_eq!(finished.load(Ordering::SeqCst), 1, "finish() must run on source end");
        let seg = out.events.iter().find_map(|e| match e {
            OceanEvent::CallTranscriptSegment { text, is_final: true, .. } => Some(text.clone()),
            _ => None,
        });
        assert_eq!(
            seg.as_deref(),
            Some("just a closing thought"),
            "the trailing final flushed on finish() must be drained into the orchestrator"
        );
        assert!(ev_types(&out).contains(&"ended"), "the call must still close");
    }

    #[tokio::test]
    async fn streaming_empty_call_brackets_cleanly() {
        // No frames, no events: the streaming loop must still emit CallStarted and
        // CallEnded — no phantom in-progress call, same contract as the batch loop.
        let (stt, rx, _p, _f) = MockStreamingStt::new(vec![], vec![]);
        let mut out = CapturingSink::default();

        run_call_session_streaming(
            session(false),
            VecFrames::new(vec![]),
            stt,
            rx,
            CannedRunner::new("hi", Default::default()),
            CapturingVoice::default(),
            &mut out,
            NoopActivitySink,
            "call:room".into(),
            vec!["sip:+1700".into()],
            UtterancePolicy::default(),
            step_clock(1_000, 10),
        )
        .await;

        assert_eq!(ev_types(&out), vec!["started", "ended"]);
    }

    #[tokio::test]
    async fn streaming_muted_call_renders_transcript_but_never_speaks() {
        // Muted: the passive transcript lane still flows off the stream, but a wake
        // command must NOT run a turn or speak — the active lane is gated, same as
        // the batch loop's muted contract.
        let during = vec![final_event("hey Ocean summarize the call", 0)];
        let (stt, rx, _p, _f) = MockStreamingStt::new(during, vec![]);
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let voice = CapturingVoice::default();
        let spoken = voice.spoken.clone();
        let mut out = CapturingSink::default();

        run_call_session_streaming(
            session(true), // muted
            VecFrames::new(vec![frame_3s()]),
            stt,
            rx,
            CannedRunner::new("should not be spoken", seen.clone()),
            voice,
            &mut out,
            NoopActivitySink,
            "call:sensitive".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        )
        .await;

        let types = ev_types(&out);
        assert!(types.contains(&"segment"), "transcript flows even when muted; got {types:?}");
        assert!(!types.contains(&"wake"), "muted call must not trigger wake; got {types:?}");
        assert!(!types.contains(&"spoke"), "muted call must not speak; got {types:?}");
        assert!(seen.lock().unwrap().is_empty(), "muted call must not run a turn");
        assert!(spoken.lock().unwrap().is_empty(), "muted call must not speak");
    }

    #[tokio::test]
    async fn streaming_dropped_source_still_closes_the_call() {
        // A mid-call transport drop must still emit CallEnded on the streaming path
        // (no phantom in-progress call), mirroring the batch loop's guarantee.
        let (stt, rx, _p, _f) = MockStreamingStt::new(vec![], vec![]);
        let mut out = CapturingSink::default();

        run_call_session_streaming(
            session(false),
            VecFrames::dropped(vec![], "PeerConnectionFailed"),
            stt,
            rx,
            CannedRunner::new("x", Default::default()),
            CapturingVoice::default(),
            &mut out,
            NoopActivitySink,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(1_000, 50),
        )
        .await;

        assert!(ev_types(&out).contains(&"ended"), "dropped streaming call must close");
    }

    // =======================================================================
    // Barge-in (OCEAN-243): human speech cancels Ocean's in-flight TTS.
    //
    // OCEAN-242 exposed the `SpeechActivity::Onset` edge at the loop boundary;
    // here we prove the real consumer works. A `BargeInCanceller` (the active-
    // lane ActivitySink) and a `BargeInVoice` (a Voice decorator) share one
    // `BargeInSignal`: the canceller trips it on `Onset`, and `BargeInVoice::speak`
    // races the inner TTS playback against it so the speak is cut the instant the
    // human talks. These tests drive that two ways:
    //   1. directly on the primitives (deterministic, no loop), and
    //   2. through `run_call_session_streaming` end-to-end, where a wake answer is
    //      mid-`speak` when an Onset event arrives off the stream.
    // =======================================================================

    /// A [`Voice`] whose `speak` **parks** the configured number of times before
    /// completing promptly — modelling a long TTS utterance still playing into the
    /// room. It records, via shared counters, how many speaks *started* vs
    /// *finished*, so a test can prove a speak was **cancelled** (started but never
    /// finished) when barge-in drops the future.
    ///
    /// `park_first` parks exactly the first N speaks (each on a `release` Notify
    /// that the barge-in tests never fire — so the only exit is cancellation);
    /// later speaks return immediately, letting a *subsequent* answer complete to
    /// completion after a barge-in. This is counter-based (not a flippable flag) so
    /// there is no driver/voice race: the first answer always parks, the second
    /// never does, deterministically.
    #[derive(Clone)]
    struct GatedVoice {
        started: Arc<AtomicU64>,
        finished: Arc<AtomicU64>,
        /// Number of leading speaks that should park (await `release`). The barge-in
        /// tests set this to 1 and never release, so the first speak only exits by
        /// being cancelled.
        park_first: Arc<AtomicU64>,
        /// Released to let a *parked* speak run to completion. Unused by the barge-in
        /// tests (which cancel instead), but lets the same mock model a normal
        /// completed utterance if a future test wants one.
        release: Arc<Notify>,
    }

    impl GatedVoice {
        /// Park the first `park_first` speaks; complete the rest immediately.
        fn new(park_first: u64) -> Self {
            Self {
                started: Arc::new(AtomicU64::new(0)),
                finished: Arc::new(AtomicU64::new(0)),
                park_first: Arc::new(AtomicU64::new(park_first)),
                release: Arc::new(Notify::new()),
            }
        }
    }

    #[async_trait]
    impl Voice for GatedVoice {
        async fn speak(&mut self, _text: &str) -> anyhow::Result<()> {
            let n = self.started.fetch_add(1, Ordering::SeqCst); // 0-based index
            if n < self.park_first.load(Ordering::SeqCst) {
                // Park until released. If barge-in cancels us, this future is
                // dropped here and `finished` is never bumped — that's the
                // observable "the speak was cut mid-utterance".
                self.release.notified().await;
            }
            self.finished.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn barge_in_signal_cancels_in_flight_speak() {
        // Unit-level proof of the cancel primitive, no session loop. A `BargeInVoice`
        // wrapping a parked `GatedVoice` is cut the instant the shared signal trips —
        // `speak` returns Ok without the inner speak ever finishing.
        let (mut canceller, signal) = BargeInCanceller::new();
        let inner = GatedVoice::new(1); // first (only) speak parks
        let started = inner.started.clone();
        let finished = inner.finished.clone();
        let mut voice = BargeInVoice::new(inner, signal);

        // Drive the parking speak on its own task so this test can observe it park
        // (started, not finished) and then trip the barge-in.
        let speak = tokio::spawn(async move {
            voice.speak("a long sentence Ocean is saying").await.unwrap();
        });

        // Let the speak reach its park point. `started` is bumped synchronously at
        // the top of `speak`, before the await, so once it reads 1 the speak is
        // parked. Yield until then (bounded by the outer #[tokio::test] watchdog).
        while started.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        assert_eq!(started.load(Ordering::SeqCst), 1, "speak must have started");
        assert_eq!(finished.load(Ordering::SeqCst), 0, "speak must not have finished yet");
        assert!(!speak.is_finished(), "the parked speak must not have returned");

        // Human starts talking → Onset → canceller trips the signal.
        canceller.on_activity(SpeechActivity::Onset);

        // The speak now resolves via the cancel arm — bounded so a regression
        // (no cancellation) fails fast instead of hanging the suite.
        tokio::time::timeout(Duration::from_secs(5), speak)
            .await
            .expect("barge-in must cancel the parked speak (it would otherwise hang)")
            .expect("speak task should not panic");
        assert_eq!(
            finished.load(Ordering::SeqCst),
            0,
            "the inner speak must have been cut mid-utterance, never completing"
        );
    }

    #[tokio::test]
    async fn barge_in_rearms_so_next_answer_speaks() {
        // After a barge-in cancels one utterance, a `Settled` (the human paused)
        // rearms the signal so the *next* answer speaks normally — the cancel is
        // per-utterance, it doesn't wedge the voice shut.
        let (mut canceller, signal) = BargeInCanceller::new();
        let inner = GatedVoice::new(0); // never parks — model the human having finished
        let started = inner.started.clone();
        let finished = inner.finished.clone();
        let mut voice = BargeInVoice::new(inner, signal.clone());

        // Onset already standing (human talking) → the (ungated) speak still races,
        // but since the voice never parks the inner completes immediately. To
        // exercise the rearm path, trip then settle, then speak.
        canceller.on_activity(SpeechActivity::Onset);
        assert!(signal.is_barged(), "Onset must trip the signal");
        canceller.on_activity(SpeechActivity::Settled);
        assert!(!signal.is_barged(), "Settled must rearm (clear) the signal");

        voice
            .speak("the next thing Ocean says")
            .await
            .expect("a rearmed voice must speak");
        assert_eq!(started.load(Ordering::SeqCst), 1, "the next answer started");
        assert_eq!(
            finished.load(Ordering::SeqCst),
            1,
            "the next answer ran to completion after rearm"
        );
    }

    #[tokio::test]
    async fn streaming_onset_cuts_ocean_mid_utterance_and_next_answer_speaks() {
        // End-to-end through the streaming loop. Script: a wake final starts an
        // answer whose `speak` parks (Ocean is talking); then an interim carrying
        // an Onset arrives — the human barged in — and must cut that speak. Then a
        // second wake final (after the human settles) must answer + speak normally.
        //
        // The driver wraps the real `BargeInCanceller`/`BargeInVoice`. The gated
        // voice parks the first speak; the loop's concurrent pump dequeues the
        // Onset while parked and trips the shared signal, cancelling it. A timeout
        // guards the whole run so a missing cancellation fails fast (the parked
        // speak would otherwise hang the call forever).
        let during = vec![
            // 1) Wake command → answer → speak (parks: Ocean talking).
            final_event("hey Ocean give me the long version", 0),
            // 2) Human barges in: leading interim of a new spurt carries Onset.
            interim_onset_event("wait actually", 1_000),
            // 3) Human settles that spurt (rearm), then a second wake command that
            //    must answer + speak normally now the voice is rearmed.
            StreamEvent {
                update: SegmentUpdate::Final(TranscriptSegment::final_(
                    "caller",
                    "wait actually never mind",
                    1_000,
                )),
                activity: Some(SpeechActivity::Settled),
            },
            final_event("hey Ocean what's next", 2_000),
        ];

        let (stt, rx, _pushes, finished_calls) = MockStreamingStt::new(during, vec![]);
        let seen: Arc<Mutex<Vec<String>>> = Default::default();

        // The gated voice parks ONLY the first speak (so the Onset can cut it);
        // answer #2 never parks, so it speaks to completion. Counter-based, so no
        // driver/voice race. Wrapped in BargeInVoice + a BargeInCanceller sharing
        // one signal — the real OCEAN-243 wiring.
        let gated = GatedVoice::new(1);
        let started = gated.started.clone();
        let finished = gated.finished.clone();
        let (canceller, signal) = BargeInCanceller::new();
        let voice = BargeInVoice::new(gated, signal);
        let mut out = CapturingSink::default();

        let run = run_call_session_streaming(
            // Near-zero echo-cooldown so the SECOND wake answer isn't suppressed as
            // Ocean's own echo on this compressed fake-clock timeline (the cooldown
            // itself is exercised elsewhere; here we want both answers to run).
            session_cooldown(false, 0),
            VecFrames::new(vec![frame_3s()]),
            stt,
            rx,
            CannedRunner::new("Here is the long version you asked for.", seen.clone()),
            voice,
            &mut out,
            canceller,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        );

        // Whole call bounded: a broken barge-in would leave answer #1 parked
        // forever, so a hang *is* the failure signal — surface it as a timeout.
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("barge-in must cut the parked speak so the call can finish (else it hangs)");

        // The call bracketed cleanly despite the mid-utterance cancellation.
        let types = ev_types(&out);
        assert_eq!(types.first(), Some(&"started"));
        assert_eq!(types.last(), Some(&"ended"));

        // Both wake answers ran. (A summary turn may also fire when the finals
        // cross the every_n_segments threshold, so filter to the wake commands —
        // the non-summary turns — and assert exactly those two, in order.)
        let seen = seen.lock().unwrap().clone();
        let wake_turns: Vec<String> = seen
            .iter()
            .filter(|p| !p.starts_with(SUMMARY_INSTRUCTION))
            .cloned()
            .collect();
        // (The wake matcher normalizes the stripped command — e.g. punctuation —
        // so assert on the normalized forms it actually produces.)
        assert_eq!(
            wake_turns,
            vec!["give me the long version".to_string(), "what s next".to_string()],
            "both wake commands drove a turn, in order; got {seen:?}"
        );

        // Two speaks STARTED (one per answer), but the first was CUT — only the
        // second ran to completion. started=2, finished=1 is the load-bearing
        // proof that barge-in cancelled the first utterance mid-stream.
        assert_eq!(started.load(Ordering::SeqCst), 2, "both answers began speaking");
        assert_eq!(
            finished.load(Ordering::SeqCst),
            1,
            "exactly one speak completed — the first was cut by barge-in, the second spoke"
        );

        // Both answers still emitted CallAgentSpoke (the rail shows what Ocean said
        // even when the audio leg was cut), and the call closed.
        let spoke_count = out
            .events
            .iter()
            .filter(|e| matches!(e, OceanEvent::CallAgentSpoke { .. }))
            .count();
        assert_eq!(spoke_count, 2, "each answer emits CallAgentSpoke; got {types:?}");
        assert_eq!(finished_calls.load(Ordering::SeqCst), 1, "finish() runs once on source end");
    }

    #[tokio::test]
    async fn streaming_no_onset_lets_ocean_finish_speaking() {
        // Control: with NO barge-in, a `BargeInVoice` answer speaks to completion —
        // the cancel path must not fire spuriously. A normal (ungated) gated voice
        // completes; finished==started==1.
        let during = vec![final_event("hey Ocean summarize that", 0)];
        let (stt, rx, _p, _f) = MockStreamingStt::new(during, vec![]);
        let seen: Arc<Mutex<Vec<String>>> = Default::default();

        let gated = GatedVoice::new(0); // never parks: a quick utterance
        let started = gated.started.clone();
        let finished = gated.finished.clone();
        let (canceller, signal) = BargeInCanceller::new();
        let voice = BargeInVoice::new(gated, signal);
        let mut out = CapturingSink::default();

        run_call_session_streaming(
            session(false),
            VecFrames::new(vec![frame_3s()]),
            stt,
            rx,
            CannedRunner::new("Here is the summary.", seen.clone()),
            voice,
            &mut out,
            canceller,
            "call:room".into(),
            vec![],
            UtterancePolicy::default(),
            step_clock(0, 100),
        )
        .await;

        let types = ev_types(&out);
        assert!(types.contains(&"spoke"), "Ocean must speak when not barged; got {types:?}");
        assert_eq!(started.load(Ordering::SeqCst), 1, "the answer spoke once");
        assert_eq!(
            finished.load(Ordering::SeqCst),
            1,
            "with no barge-in the speak must run to completion (no spurious cancel)"
        );
    }
}
