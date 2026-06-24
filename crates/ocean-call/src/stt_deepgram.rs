//! Deepgram streaming STT adapter behind the [`SttProvider`] trait.
//!
//! Unlike the batch [`crate::stt_xai`] adapter (which buffers a whole utterance
//! then POSTs a WAV and only yields a transcript *after* the speaker pauses),
//! this provider keeps a **live WebSocket** open and streams 20ms PCM frames as
//! they arrive, so interim hypotheses come back in real time. That is what makes
//! a call feel live and what makes **barge-in** possible: the moment the human
//! starts talking, Deepgram emits a non-empty interim, and we raise a
//! [`SpeechActivity::Onset`] the active lane can use to cut Ocean's TTS.
//!
//! ## Deepgram wire protocol (verified against developers.deepgram.com)
//!
//! ```text
//! Connect:  wss://api.deepgram.com/v1/listen
//!             ?encoding=linear16&sample_rate=16000&channels=1
//!             &interim_results=true&model=nova-3&punctuate=true
//!           Header: Authorization: Token <DEEPGRAM_API_KEY>
//! Audio:    raw little-endian i16 PCM as BINARY WebSocket frames (20–250ms each)
//! Results:  TEXT frames, JSON:
//!           { "type":"Results", "start":0.0, "duration":1.5,
//!             "is_final":true, "speech_final":true,
//!             "channel": { "alternatives": [ { "transcript":"...", ... } ] } }
//! Close:    send TEXT {"type":"CloseStream"} to flush + finish.
//! ```
//!
//! ## What is tested vs gated
//!
//! Everything that shapes bytes or parses JSON is **pure** and unit-tested here
//! with zero network: [`build_ws_url`], [`pcm_to_le_bytes`], [`parse_results`]
//! (interim/final classification), and [`CLOSE_STREAM_MSG`]. Only the live
//! WebSocket connection in [`live`] needs a runtime `DEEPGRAM_API_KEY`, and it is
//! gated behind the `deepgram-stt` feature so the default build pulls no
//! websocket stack, no credentials, no network.

use crate::frame::PcmFrame;
use crate::stt::TranscriptSegment;

/// Deepgram streaming endpoint. PCM frames go in as binary; `Results` come back
/// as JSON text.
pub const DEEPGRAM_WS_URL: &str = "wss://api.deepgram.com/v1/listen";

/// Default Deepgram model. `nova-3` is the current general streaming model; the
/// config lets the daemon override it without touching this crate.
pub const DEEPGRAM_DEFAULT_MODEL: &str = "nova-3";

/// The JSON control message that tells Deepgram to flush pending audio and emit
/// the trailing final, then close. Sent as a TEXT frame on [`SttProvider::finish`].
pub const CLOSE_STREAM_MSG: &str = r#"{"type":"CloseStream"}"#;

/// Diarization-free speaker label used for streaming segments. Deepgram can
/// diarize (`diarize=true`), but the call lane currently labels by LiveKit
/// participant downstream, so a single stable label keeps the assembler's
/// per-speaker tracking coherent. Matches the batch adapter's "caller".
pub const STREAM_SPEAKER: &str = "caller";

/// Connection + decode parameters for the Deepgram stream. Pure config — holds
/// no key and opens no socket; [`build_ws_url`] turns it into the query string.
#[derive(Debug, Clone)]
pub struct DeepgramConfig {
    /// PCM sample rate (Hz). Must match the frames pushed in — the call lane is
    /// 16kHz mono.
    pub sample_rate: u32,
    /// PCM channel count. 1 (mono) for the call lane.
    pub channels: u32,
    /// Deepgram model id (e.g. "nova-3").
    pub model: String,
    /// Ask Deepgram for low-latency interim hypotheses, not just finals. This is
    /// what feeds real-time rendering and the barge-in onset signal.
    pub interim_results: bool,
    /// Add punctuation/casing to transcripts.
    pub punctuate: bool,
}

impl Default for DeepgramConfig {
    fn default() -> Self {
        Self {
            sample_rate: 16_000,
            channels: 1,
            model: DEEPGRAM_DEFAULT_MODEL.to_string(),
            interim_results: true,
            punctuate: true,
        }
    }
}

impl DeepgramConfig {
    /// Match the format of a [`PcmFrame`] (rate + channels) so the URL we open
    /// agrees with the bytes we'll send. Model/flags keep their defaults.
    pub fn for_frame(frame: &PcmFrame) -> Self {
        Self {
            sample_rate: frame.sample_rate,
            channels: frame.num_channels.max(1),
            ..Self::default()
        }
    }
}

/// Build the full `wss://…/v1/listen?…` URL for `cfg`. Pure string assembly so
/// the exact query Deepgram receives is unit-testable without a socket.
pub fn build_ws_url(cfg: &DeepgramConfig) -> String {
    format!(
        "{base}?encoding=linear16&sample_rate={sr}&channels={ch}&interim_results={interim}&punctuate={punct}&model={model}",
        base = DEEPGRAM_WS_URL,
        sr = cfg.sample_rate,
        ch = cfg.channels.max(1),
        interim = cfg.interim_results,
        punct = cfg.punctuate,
        model = cfg.model,
    )
}

/// Encode one frame's interleaved i16 samples as the little-endian byte buffer
/// Deepgram expects for `encoding=linear16`. No WAV header — the stream is raw
/// PCM, framed by the WebSocket itself.
pub fn pcm_to_le_bytes(frame: &PcmFrame) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.data.len() * 2);
    for s in &frame.data {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Speech-activity edge derived from the transcript stream — the **barge-in
/// seam**. The streaming provider raises [`Onset`](SpeechActivity::Onset) the
/// first time a non-empty interim appears after a quiet stretch, which the
/// active lane uses to stop Ocean's TTS the instant the human starts talking.
///
/// This is intentionally minimal: just the *signal*. The provider emits it; the
/// orchestrator decides what "stop TTS" means. Full duck/cancel orchestration is
/// a follow-up — this lands the seam without entangling TTS here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechActivity {
    /// The human just started speaking (first interim after silence). Cut TTS.
    Onset,
    /// A final settled and no interim is outstanding — the speaker paused. The
    /// lane may resume/allow Ocean speech.
    Settled,
}

/// Tracks whether speech is currently "open" (an interim is outstanding) so the
/// provider can derive [`SpeechActivity`] edges from the flat segment stream.
///
/// Deepgram sends a run of interims then a final per utterance. We want exactly
/// one [`Onset`](SpeechActivity::Onset) at the *leading* edge of a talk-spurt
/// (not on every interim) and one [`Settled`](SpeechActivity::Settled) when the
/// final closes it. Pure + unit-tested; no socket, no timers.
#[derive(Debug, Default)]
pub struct SpeechActivityTracker {
    /// True once we've raised Onset for the current spurt, until a final clears it.
    speaking: bool,
}

impl SpeechActivityTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a parsed segment; returns a [`SpeechActivity`] edge iff this segment
    /// flips the speaking state. Empty-text segments never move the edge.
    pub fn observe(&mut self, seg: &TranscriptSegment) -> Option<SpeechActivity> {
        if seg.text.trim().is_empty() {
            return None;
        }
        if seg.is_final {
            // A final closes the current spurt. Only a falling edge if we were
            // actually speaking.
            if self.speaking {
                self.speaking = false;
                return Some(SpeechActivity::Settled);
            }
            None
        } else {
            // Interim: rising edge only on the first one of a spurt.
            if !self.speaking {
                self.speaking = true;
                return Some(SpeechActivity::Onset);
            }
            None
        }
    }

    /// True while a talk-spurt is open (an interim is outstanding).
    pub fn is_speaking(&self) -> bool {
        self.speaking
    }

    /// Reset state (e.g. on reconnect / call end).
    pub fn reset(&mut self) {
        self.speaking = false;
    }
}

/// Parse one Deepgram WebSocket message body into a [`TranscriptSegment`].
///
/// Returns:
/// - `Ok(Some(seg))` for a `Results` message carrying a transcript — `is_final`
///   from the payload maps to [`TranscriptSegment::is_final`], `start` (seconds)
///   to `start_ms`. The speaker is [`STREAM_SPEAKER`].
/// - `Ok(None)` for non-transcript control frames (`Metadata`,
///   `SpeechStarted`, `UtteranceEnd`, a `Results` with empty/whitespace
///   transcript, …) — nothing to surface, but not an error.
/// - `Err(_)` only if the bytes aren't valid JSON.
///
/// `base_ms` is the call-relative time of the stream's first audio, added to
/// Deepgram's stream-relative `start` so segment timestamps line up with the
/// rest of the call timeline.
pub fn parse_results(body: &[u8], base_ms: u64) -> anyhow::Result<Option<TranscriptSegment>> {
    let v: serde_json::Value = serde_json::from_slice(body)?;

    // Only `Results` frames carry transcripts. If `type` is present and is
    // something else, it's a control frame → ignore. If `type` is absent we
    // still try to read a transcript (be lenient about minor schema drift).
    if let Some(t) = v.get("type").and_then(|t| t.as_str()) {
        if t != "Results" {
            return Ok(None);
        }
    }

    let transcript = v
        .get("channel")
        .and_then(|c| c.get("alternatives"))
        .and_then(|a| a.as_array())
        .and_then(|alts| alts.first())
        .and_then(|alt| alt.get("transcript"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim();

    if transcript.is_empty() {
        return Ok(None);
    }

    let is_final = v.get("is_final").and_then(|f| f.as_bool()).unwrap_or(false);

    // `start` is seconds-from-stream-start as a float; convert to ms and offset
    // by the call-relative base so timelines align.
    let start_ms = v
        .get("start")
        .and_then(|s| s.as_f64())
        .map(|secs| (secs * 1000.0).round() as u64)
        .unwrap_or(0)
        .saturating_add(base_ms);

    let seg = if is_final {
        TranscriptSegment::final_(STREAM_SPEAKER, transcript, start_ms)
    } else {
        TranscriptSegment::interim(STREAM_SPEAKER, transcript, start_ms)
    };
    Ok(Some(seg))
}

// ---------------------------------------------------------------------------
// Live WebSocket provider — the only part that opens a socket / needs a key.
// Gated behind `deepgram-stt` so the default build stays clean: no tungstenite,
// no TLS-for-ws, no network. Everything above is always compiled + tested.
// ---------------------------------------------------------------------------
#[cfg(feature = "deepgram-stt")]
pub mod live {
    use super::*;
    use std::sync::Arc;

    use async_trait::async_trait;
    use futures_util::{SinkExt, StreamExt};
    use tokio::sync::{mpsc, Mutex};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message;

    use crate::stt::{SegmentAssembler, SegmentUpdate, StreamEvent, SttProvider};

    /// Live Deepgram streaming provider. Owns the write half of the socket (to
    /// push PCM) behind a mutex; a spawned read pump turns incoming `Results`
    /// into [`StreamEvent`]s on `events_rx`, applying the [`SegmentAssembler`]
    /// (interim de-dup) and the [`SpeechActivityTracker`] (barge-in edges).
    ///
    /// Construct with [`connect`]. The provider satisfies [`SttProvider`] so it
    /// drops into the call lane wherever a streaming STT is expected, with the
    /// **same trait** the batch adapter would use — no orchestrator change.
    pub struct DeepgramStt {
        write: Arc<Mutex<WsWrite>>,
        /// Call-relative ms of the first frame, stamped onto segments. Set on the
        /// first `push_frame` so timestamps are call-aligned, not socket-aligned.
        base_ms: Arc<Mutex<Option<u64>>>,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
        started_ms: u64,
    }

    type WsStream = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;
    type WsWrite = futures_util::stream::SplitSink<WsStream, Message>;

    impl DeepgramStt {
        /// Open a Deepgram stream for `cfg`, authenticating with `api_key`.
        ///
        /// Returns the provider plus the receiver of [`StreamEvent`]s the read
        /// pump produces. The pump runs until the socket closes (Deepgram's
        /// response to `CloseStream`, or a drop). `clock` supplies call-relative
        /// milliseconds for base-time stamping; pass the same clock the session
        /// loop uses so transcripts share its timeline.
        pub async fn connect(
            cfg: &DeepgramConfig,
            api_key: &str,
            clock: Arc<dyn Fn() -> u64 + Send + Sync>,
            // Call-start epoch (the caller's `clock()` at session start). Without
            // it, `now_rel()` subtracts 0 and the first frame stamps `base_ms`
            // with the full wall-clock epoch, so every streaming segment's
            // `start_ms` is ~epoch-scale instead of call-relative (the batch path
            // in session_task.rs captures `started_ms` the same way).
            started_ms: u64,
        ) -> anyhow::Result<(Self, mpsc::UnboundedReceiver<StreamEvent>)> {
            let url = build_ws_url(cfg);
            let mut request = url.into_client_request()?;
            request
                .headers_mut()
                .insert("Authorization", format!("Token {api_key}").parse()?);

            let (ws, _resp) = tokio_tungstenite::connect_async(request).await?;
            let (write, mut read) = ws.split();

            let (events_tx, events_rx) = mpsc::unbounded_channel::<StreamEvent>();
            let base_ms: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
            let base_for_pump = base_ms.clone();

            // Read pump: parse every text frame, classify, derive barge-in edges,
            // forward to the lane. Lives until the socket ends.
            tokio::spawn(async move {
                let mut assembler = SegmentAssembler::new();
                let mut activity = SpeechActivityTracker::new();
                while let Some(msg) = read.next().await {
                    let msg = match msg {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::warn!(error = %e, "deepgram ws read error; ending pump");
                            break;
                        }
                    };
                    let body = match &msg {
                        Message::Text(t) => t.as_bytes().to_vec(),
                        Message::Binary(b) => b.to_vec(),
                        Message::Close(_) => {
                            tracing::debug!("deepgram ws closed by server");
                            break;
                        }
                        // Ping/Pong/Frame: tungstenite auto-handles pings; ignore.
                        _ => continue,
                    };
                    let base = base_for_pump.lock().await.unwrap_or(0);
                    let seg = match parse_results(&body, base) {
                        Ok(Some(seg)) => seg,
                        Ok(None) => continue,
                        Err(e) => {
                            tracing::debug!(error = %e, "deepgram non-JSON frame ignored");
                            continue;
                        }
                    };
                    // Barge-in edge is derived from the raw segment (before
                    // de-dup) so the onset isn't swallowed by a duplicate interim.
                    let edge = activity.observe(&seg);
                    let update = assembler.ingest(seg);
                    if matches!(update, SegmentUpdate::Ignore) && edge.is_none() {
                        continue;
                    }
                    if events_tx
                        .send(StreamEvent {
                            update,
                            activity: edge,
                        })
                        .is_err()
                    {
                        // Receiver dropped → lane gone; stop pumping.
                        break;
                    }
                }
            });

            Ok((
                Self {
                    write: Arc::new(Mutex::new(write)),
                    base_ms,
                    clock,
                    started_ms,
                },
                events_rx,
            ))
        }

        /// Wall-to-call-relative ms using the provided clock.
        fn now_rel(&self) -> u64 {
            (self.clock)().saturating_sub(self.started_ms)
        }
    }

    #[async_trait]
    impl SttProvider for DeepgramStt {
        async fn push_frame(&self, frame: PcmFrame) -> anyhow::Result<()> {
            // Stamp the call-relative base from the first frame so segment
            // timestamps align with the session timeline.
            {
                let mut base = self.base_ms.lock().await;
                if base.is_none() {
                    *base = Some(self.now_rel());
                }
            }
            let bytes = pcm_to_le_bytes(&frame);
            let mut w = self.write.lock().await;
            w.send(Message::Binary(bytes)).await?;
            Ok(())
        }

        async fn finish(&self) -> anyhow::Result<()> {
            let mut w = self.write.lock().await;
            // Ask Deepgram to flush + emit the trailing final, then close our
            // write half so the pump sees the server's Close and exits.
            w.send(Message::Text(CLOSE_STREAM_MSG.into())).await?;
            w.close().await?;
            Ok(())
        }

        fn name(&self) -> &'static str {
            "deepgram"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_carries_linear16_and_format() {
        let cfg = DeepgramConfig {
            sample_rate: 16_000,
            channels: 1,
            model: "nova-3".into(),
            interim_results: true,
            punctuate: true,
        };
        let url = build_ws_url(&cfg);
        assert!(url.starts_with("wss://api.deepgram.com/v1/listen?"));
        assert!(url.contains("encoding=linear16"), "{url}");
        assert!(url.contains("sample_rate=16000"), "{url}");
        assert!(url.contains("channels=1"), "{url}");
        assert!(url.contains("interim_results=true"), "{url}");
        assert!(url.contains("punctuate=true"), "{url}");
        assert!(url.contains("model=nova-3"), "{url}");
    }

    #[test]
    fn url_reflects_stereo_and_custom_model() {
        let cfg = DeepgramConfig {
            sample_rate: 48_000,
            channels: 2,
            model: "nova-2-phonecall".into(),
            interim_results: false,
            punctuate: false,
        };
        let url = build_ws_url(&cfg);
        assert!(url.contains("sample_rate=48000"), "{url}");
        assert!(url.contains("channels=2"), "{url}");
        assert!(url.contains("interim_results=false"), "{url}");
        assert!(url.contains("model=nova-2-phonecall"), "{url}");
    }

    #[test]
    fn config_for_frame_matches_audio_format() {
        let frame = PcmFrame::new(vec![0i16; 320], 16_000, 1);
        let cfg = DeepgramConfig::for_frame(&frame);
        assert_eq!(cfg.sample_rate, 16_000);
        assert_eq!(cfg.channels, 1);
        // Defaults preserved.
        assert!(cfg.interim_results);
        assert_eq!(cfg.model, DEEPGRAM_DEFAULT_MODEL);
    }

    #[test]
    fn pcm_encodes_little_endian_i16() {
        // 1 = 0x0001 → [0x01, 0x00]; -1 = 0xFFFF → [0xFF, 0xFF]; 256 → [0x00,0x01].
        let frame = PcmFrame::new(vec![1, -1, 256], 16_000, 1);
        let bytes = pcm_to_le_bytes(&frame);
        assert_eq!(bytes, vec![0x01, 0x00, 0xFF, 0xFF, 0x00, 0x01]);
        // Byte length is exactly 2 per sample.
        assert_eq!(bytes.len(), frame.data.len() * 2);
    }

    #[test]
    fn close_stream_message_is_the_documented_json() {
        let v: serde_json::Value = serde_json::from_str(CLOSE_STREAM_MSG).unwrap();
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("CloseStream"));
    }

    #[test]
    fn parse_final_results_yields_final_segment() {
        let body = br#"{
            "type": "Results",
            "start": 1.5,
            "duration": 1.2,
            "is_final": true,
            "speech_final": true,
            "channel": { "alternatives": [ { "transcript": "let's ship friday", "confidence": 0.97 } ] }
        }"#;
        let seg = parse_results(body, 0).unwrap().unwrap();
        assert!(seg.is_final);
        assert_eq!(seg.text, "let's ship friday");
        assert_eq!(seg.speaker, STREAM_SPEAKER);
        // start 1.5s → 1500ms.
        assert_eq!(seg.start_ms, 1_500);
    }

    #[test]
    fn parse_interim_results_yields_interim_segment() {
        let body = br#"{
            "type": "Results",
            "start": 0.4,
            "is_final": false,
            "channel": { "alternatives": [ { "transcript": "let's" } ] }
        }"#;
        let seg = parse_results(body, 0).unwrap().unwrap();
        assert!(!seg.is_final, "is_final:false must classify as interim");
        assert_eq!(seg.text, "let's");
        assert_eq!(seg.start_ms, 400);
    }

    #[test]
    fn parse_offsets_start_by_call_base_ms() {
        let body = br#"{
            "type": "Results",
            "start": 2.0,
            "is_final": true,
            "channel": { "alternatives": [ { "transcript": "ok" } ] }
        }"#;
        // Stream opened 10s into the call → segment lands at 12_000ms.
        let seg = parse_results(body, 10_000).unwrap().unwrap();
        assert_eq!(seg.start_ms, 12_000);
    }

    #[test]
    fn parse_empty_transcript_is_none() {
        let body = br#"{
            "type": "Results",
            "is_final": true,
            "channel": { "alternatives": [ { "transcript": "   " } ] }
        }"#;
        assert!(parse_results(body, 0).unwrap().is_none());
    }

    #[test]
    fn parse_metadata_control_frame_is_none() {
        // Non-Results control frames (Metadata, SpeechStarted, UtteranceEnd) carry
        // no transcript and must be ignored, not error.
        let body = br#"{ "type": "Metadata", "request_id": "abc", "duration": 3.0 }"#;
        assert!(parse_results(body, 0).unwrap().is_none());
        let started = br#"{ "type": "SpeechStarted", "timestamp": 1.0 }"#;
        assert!(parse_results(started, 0).unwrap().is_none());
    }

    #[test]
    fn parse_missing_alternatives_is_none() {
        let body =
            br#"{ "type": "Results", "is_final": false, "channel": { "alternatives": [] } }"#;
        assert!(parse_results(body, 0).unwrap().is_none());
    }

    #[test]
    fn parse_rejects_non_json() {
        assert!(parse_results(b"not json at all", 0).is_err());
    }

    #[test]
    fn parse_default_is_final_false_when_absent() {
        // A Results frame without is_final defaults to interim (revisable), per
        // the "never block the lane — mark final:false" rule.
        let body =
            br#"{ "type": "Results", "channel": { "alternatives": [ { "transcript": "hi" } ] } }"#;
        let seg = parse_results(body, 0).unwrap().unwrap();
        assert!(!seg.is_final);
    }

    // ---- barge-in seam ----

    #[test]
    fn first_interim_raises_onset() {
        let mut t = SpeechActivityTracker::new();
        let seg = TranscriptSegment::interim("caller", "hey", 0);
        assert_eq!(t.observe(&seg), Some(SpeechActivity::Onset));
        assert!(t.is_speaking());
    }

    #[test]
    fn subsequent_interims_do_not_re_raise_onset() {
        let mut t = SpeechActivityTracker::new();
        t.observe(&TranscriptSegment::interim("caller", "hey", 0));
        // Growing interim within the same spurt → no new edge.
        assert_eq!(
            t.observe(&TranscriptSegment::interim("caller", "hey ocean", 0)),
            None
        );
    }

    #[test]
    fn final_after_interims_raises_settled() {
        let mut t = SpeechActivityTracker::new();
        t.observe(&TranscriptSegment::interim("caller", "hey", 0));
        t.observe(&TranscriptSegment::interim("caller", "hey ocean", 0));
        assert_eq!(
            t.observe(&TranscriptSegment::final_("caller", "hey ocean", 0)),
            Some(SpeechActivity::Settled)
        );
        assert!(!t.is_speaking());
    }

    #[test]
    fn final_without_prior_interim_yields_no_edge() {
        // A standalone final (no spurt was open) shouldn't claim a falling edge.
        let mut t = SpeechActivityTracker::new();
        assert_eq!(
            t.observe(&TranscriptSegment::final_("caller", "ok", 0)),
            None
        );
    }

    #[test]
    fn empty_segment_moves_no_edge() {
        let mut t = SpeechActivityTracker::new();
        assert_eq!(
            t.observe(&TranscriptSegment::interim("caller", "   ", 0)),
            None
        );
        assert!(!t.is_speaking());
    }

    #[test]
    fn onset_then_settled_then_onset_again_cycles() {
        // A second talk-spurt after a settled final must raise Onset again — the
        // barge-in signal has to fire on every leading edge, not just the first.
        let mut t = SpeechActivityTracker::new();
        assert_eq!(
            t.observe(&TranscriptSegment::interim("caller", "one", 0)),
            Some(SpeechActivity::Onset)
        );
        assert_eq!(
            t.observe(&TranscriptSegment::final_("caller", "one", 0)),
            Some(SpeechActivity::Settled)
        );
        assert_eq!(
            t.observe(&TranscriptSegment::interim("caller", "two", 100)),
            Some(SpeechActivity::Onset)
        );
    }
}
