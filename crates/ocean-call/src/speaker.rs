//! speaker — publish Ocean's TTS voice back into the call room (active lane).
//!
//! The inverse of [`crate::room_tap`]: instead of subscribing to remote audio,
//! the active lane *publishes* a local audio track and pushes synthesized TTS
//! PCM into it. The verified API chain (livekit 0.7.44 / libwebrtc 0.3.35,
//! whose own docs label this path "Manual audio (TTS, files)"):
//!
//! ```text
//! NativeAudioSource::new(opts, sample_rate, num_channels, queue_ms)   // queue_ms %10==0
//!   -> RtcAudioSource::Native(src)
//!   -> LocalAudioTrack::create_audio_track("ocean-voice", source)
//!   -> room.local_participant().publish_track(track, opts)
//!   -> src.capture_frame(&AudioFrame{ data, sample_rate, num_channels, .. }).await  // push TTS PCM
//! ```
//!
//! `capture_frame` wants fixed small frames (10ms is the WebRTC native size).
//! [`PcmPacer`] is the pure, off-device-testable bridge: take an arbitrary TTS
//! PCM buffer and yield exact 10ms [`crate::frame::PcmFrame`]s to feed it. The
//! live publish loop is behind the `livekit-tap` feature (native WebRTC); the
//! pacing logic is always built and tested.

use crate::frame::PcmFrame;

/// WebRTC native audio frame size. `capture_frame` is happiest fed 10ms frames.
pub const FRAME_MS: u32 = 10;

/// Slices a TTS PCM buffer into exact 10ms frames for `capture_frame`.
///
/// A reused [`crate::frame::FrameChunker`] handles arbitrary→fixed windowing;
/// this thin wrapper fixes the window at 10ms and the intent at "outbound TTS",
/// keeping the speaker module self-describing.
#[derive(Debug)]
pub struct PcmPacer {
    chunker: crate::frame::FrameChunker,
}

impl PcmPacer {
    pub fn new() -> Self {
        Self {
            chunker: crate::frame::FrameChunker::new(FRAME_MS),
        }
    }

    /// Feed a TTS PCM buffer; get back the complete 10ms frames it contains.
    pub fn push(&mut self, pcm: &PcmFrame) -> Vec<PcmFrame> {
        self.chunker.push(pcm)
    }

    /// Flush any sub-frame tail at end of utterance (padded by the caller if
    /// the source needs an exact frame).
    pub fn flush(&mut self) -> Option<PcmFrame> {
        self.chunker.flush()
    }
}

impl Default for PcmPacer {
    fn default() -> Self {
        Self::new()
    }
}

/// Pad a short final frame up to a full 10ms window with silence, so the source
/// always receives exactly-sized frames. No-op if already full or longer.
pub fn pad_to_frame(frame: &PcmFrame) -> PcmFrame {
    let want = (frame.sample_rate / 1000) * FRAME_MS * frame.num_channels.max(1);
    let mut data = frame.data.clone();
    if (data.len() as u32) < want {
        data.resize(want as usize, 0);
    }
    PcmFrame::new(data, frame.sample_rate, frame.num_channels)
}

// Live publish loop — native WebRTC, behind `livekit-tap`.
#[cfg(feature = "livekit-tap")]
pub mod live {
    use super::*;
    use livekit::options::TrackPublishOptions;
    use livekit::track::{LocalAudioTrack, LocalTrack, TrackSource};
    use livekit::webrtc::audio_source::native::NativeAudioSource;
    use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
    use livekit::webrtc::prelude::AudioFrame;
    use livekit::Room;

    /// Publishes a local "ocean-voice" track and returns the source to push TTS
    /// PCM into. The source is 16kHz mono with a 100ms queue (multiple of 10).
    pub async fn publish_voice_track(
        room: &Room,
        sample_rate: u32,
        num_channels: u32,
    ) -> anyhow::Result<NativeAudioSource> {
        let source = NativeAudioSource::new(
            AudioSourceOptions::default(),
            sample_rate,
            num_channels,
            100, // queue_size_ms, %10==0 (asserted by the SDK)
        );
        let track = LocalAudioTrack::create_audio_track(
            "ocean-voice",
            RtcAudioSource::Native(source.clone()),
        );
        room.local_participant()
            .publish_track(
                LocalTrack::Audio(track),
                TrackPublishOptions {
                    source: TrackSource::Microphone,
                    ..Default::default()
                },
            )
            .await?;
        Ok(source)
    }

    /// Push a full TTS PCM utterance into the room, paced into 10ms frames.
    ///
    /// **Barge-in cancellation (OCEAN-243):** this future is *cancel-safe* — the
    /// utterance is published one 10ms `capture_frame` at a time, awaiting each, so
    /// dropping the future between frames simply stops publishing. No partial-frame
    /// corruption is possible (each `capture_frame` is atomic) and there is no
    /// shared state left half-written. `BargeInVoice` relies on exactly this: when
    /// a barge-in `Onset` fires it drops the in-flight `speak` future, cutting
    /// Ocean off mid-utterance with no further audio reaching the room.
    pub async fn speak_pcm(
        source: &NativeAudioSource,
        pcm: PcmFrame,
    ) -> anyhow::Result<()> {
        let mut pacer = PcmPacer::new();
        for frame in pacer.push(&pcm) {
            capture(source, &frame).await?;
        }
        if let Some(tail) = pacer.flush() {
            capture(source, &pad_to_frame(&tail)).await?;
        }
        Ok(())
    }

    async fn capture(source: &NativeAudioSource, frame: &PcmFrame) -> anyhow::Result<()> {
        let af = AudioFrame {
            data: std::borrow::Cow::Borrowed(&frame.data),
            sample_rate: frame.sample_rate,
            num_channels: frame.num_channels,
            samples_per_channel: frame.samples_per_channel,
        };
        source
            .capture_frame(&af)
            .await
            .map_err(|e| anyhow::anyhow!("capture_frame: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacer_yields_10ms_frames_at_16k() {
        // 16kHz mono → 10ms = 160 samples. A 500-sample buffer = 3 full frames.
        let mut pacer = PcmPacer::new();
        let out = pacer.push(&PcmFrame::new(vec![1i16; 500], 16_000, 1));
        assert_eq!(out.len(), 3);
        for f in &out {
            assert_eq!(f.samples_per_channel, 160);
            assert_eq!(f.duration_ms(), 10);
        }
        // 20 left over → flushable tail.
        let tail = pacer.flush().unwrap();
        assert_eq!(tail.data.len(), 20);
    }

    #[test]
    fn pad_fills_short_frame_with_silence() {
        // 50 samples at 16k mono; a 10ms frame is 160.
        let short = PcmFrame::new(vec![7i16; 50], 16_000, 1);
        let padded = pad_to_frame(&short);
        assert_eq!(padded.data.len(), 160);
        // Original samples preserved, rest zero.
        assert_eq!(&padded.data[..50], &[7i16; 50]);
        assert!(padded.data[50..].iter().all(|&s| s == 0));
    }

    #[test]
    fn pad_is_noop_when_already_full() {
        let full = PcmFrame::new(vec![3i16; 160], 16_000, 1);
        let padded = pad_to_frame(&full);
        assert_eq!(padded.data.len(), 160);
    }

    #[test]
    fn frame_ms_is_multiple_of_ten() {
        // The SDK asserts queue_size_ms % 10 == 0; our frame size matches.
        assert_eq!(FRAME_MS % 10, 0);
    }
}
