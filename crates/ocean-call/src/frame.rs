//! PCM audio frames and fixed-window re-chunking.
//!
//! `room_tap` subscribes to a LiveKit `RemoteAudioTrack` and drives a
//! `livekit::webrtc::audio_stream::native::NativeAudioStream`, which yields
//! `AudioFrame { data: Cow<[i16]>, sample_rate, num_channels, samples_per_channel }`
//! (verified against libwebrtc 0.3.35). Those frames arrive at whatever cadence
//! WebRTC delivers them — not necessarily the fixed 20ms windows the STT lane
//! wants. [`FrameChunker`] is the pure, off-device-testable bridge: feed it the
//! variable-size frames, get back uniform 20ms [`PcmFrame`]s.
//!
//! Keeping this logic free of any livekit/webrtc type means it can be unit
//! tested without audio hardware or a live room — the room_tap adapter is a
//! thin shell that converts `AudioFrame` → [`PcmFrame`] and pumps the chunker.

/// A chunk of mono-or-multi-channel 16-bit PCM audio.
///
/// Mirrors the shape of libwebrtc's `AudioFrame` but owns its samples and
/// carries no borrowed lifetime, so it crosses async/task boundaries freely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmFrame {
    /// Interleaved i16 samples, `samples_per_channel * num_channels` long.
    pub data: Vec<i16>,
    pub sample_rate: u32,
    pub num_channels: u32,
    /// Samples per channel in this frame (`data.len() / num_channels`).
    pub samples_per_channel: u32,
}

impl PcmFrame {
    /// Build a frame from interleaved samples, deriving samples_per_channel.
    pub fn new(data: Vec<i16>, sample_rate: u32, num_channels: u32) -> Self {
        let channels = num_channels.max(1);
        let samples_per_channel = (data.len() as u32) / channels;
        Self {
            data,
            sample_rate,
            num_channels: channels,
            samples_per_channel,
        }
    }

    /// Duration of this frame in milliseconds.
    pub fn duration_ms(&self) -> u32 {
        if self.sample_rate == 0 {
            return 0;
        }
        (self.samples_per_channel * 1000) / self.sample_rate
    }
}

/// Re-slices a stream of arbitrary-length [`PcmFrame`]s into fixed-duration
/// windows (default 20ms). Sample format (rate, channels) is locked to the
/// first frame seen; frames that disagree are rejected so a mid-call format
/// change can't silently corrupt the window math.
#[derive(Debug)]
pub struct FrameChunker {
    window_ms: u32,
    sample_rate: u32,
    num_channels: u32,
    /// Interleaved samples not yet emitted in a full window.
    buf: Vec<i16>,
    /// Interleaved samples that make up one full window.
    window_len: usize,
}

impl FrameChunker {
    /// Create a chunker that emits `window_ms`-long frames. Format is adopted
    /// from the first pushed frame.
    pub fn new(window_ms: u32) -> Self {
        Self {
            window_ms: window_ms.max(1),
            sample_rate: 0,
            num_channels: 0,
            buf: Vec::new(),
            window_len: 0,
        }
    }

    /// 20ms is the canonical Opus/WebRTC frame size and what the STT lane wants.
    pub fn new_20ms() -> Self {
        Self::new(20)
    }

    /// The fixed window size, in samples-per-channel, once format is known.
    pub fn window_samples_per_channel(&self) -> u32 {
        (self.sample_rate / 1000) * self.window_ms
    }

    /// Feed one frame; returns zero or more complete fixed-size windows.
    ///
    /// A frame whose (sample_rate, num_channels) disagrees with the locked
    /// format is dropped and yields nothing — never a panic, never silent
    /// resampling.
    pub fn push(&mut self, frame: &PcmFrame) -> Vec<PcmFrame> {
        if self.sample_rate == 0 {
            // Lock format from the first frame.
            self.sample_rate = frame.sample_rate;
            self.num_channels = frame.num_channels.max(1);
            let spc = (self.sample_rate / 1000) * self.window_ms;
            self.window_len = spc as usize * self.num_channels as usize;
        } else if frame.sample_rate != self.sample_rate || frame.num_channels != self.num_channels {
            return Vec::new();
        }
        if self.window_len == 0 {
            return Vec::new();
        }

        self.buf.extend_from_slice(&frame.data);

        let mut out = Vec::new();
        while self.buf.len() >= self.window_len {
            let chunk: Vec<i16> = self.buf.drain(0..self.window_len).collect();
            out.push(PcmFrame::new(chunk, self.sample_rate, self.num_channels));
        }
        out
    }

    /// Drain any buffered tail as a final (possibly short) frame, e.g. at call
    /// end. Returns None if nothing is buffered.
    pub fn flush(&mut self) -> Option<PcmFrame> {
        if self.buf.is_empty() {
            return None;
        }
        let tail = std::mem::take(&mut self.buf);
        Some(PcmFrame::new(tail, self.sample_rate, self.num_channels))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_ms_from_sample_count() {
        // 320 samples/channel @ 16kHz = 20ms.
        let f = PcmFrame::new(vec![0i16; 320], 16_000, 1);
        assert_eq!(f.samples_per_channel, 320);
        assert_eq!(f.duration_ms(), 20);
    }

    #[test]
    fn chunker_emits_exact_20ms_windows_at_16k_mono() {
        // 16kHz mono → 20ms window = 320 samples.
        let mut chunker = FrameChunker::new_20ms();
        // Push a 1000-sample frame: yields 3 full windows (960), 40 left over.
        let out = chunker.push(&PcmFrame::new(vec![1i16; 1000], 16_000, 1));
        assert_eq!(out.len(), 3);
        for w in &out {
            assert_eq!(w.samples_per_channel, 320);
            assert_eq!(w.duration_ms(), 20);
        }
        // Next push of 280 completes the 4th window (40 + 280 = 320).
        let out2 = chunker.push(&PcmFrame::new(vec![1i16; 280], 16_000, 1));
        assert_eq!(out2.len(), 1);
        assert_eq!(out2[0].samples_per_channel, 320);
    }

    #[test]
    fn chunker_handles_stereo_interleaving() {
        // 48kHz stereo → 20ms = 960 samples/channel = 1920 interleaved.
        let mut chunker = FrameChunker::new_20ms();
        let out = chunker.push(&PcmFrame::new(vec![7i16; 1920], 48_000, 2));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].num_channels, 2);
        assert_eq!(out[0].samples_per_channel, 960);
        assert_eq!(out[0].data.len(), 1920);
    }

    #[test]
    fn mismatched_format_is_dropped_not_panicked() {
        let mut chunker = FrameChunker::new_20ms();
        // Lock to 16kHz mono.
        chunker.push(&PcmFrame::new(vec![1i16; 320], 16_000, 1));
        // A 48kHz frame disagrees → dropped, no output, no panic.
        let out = chunker.push(&PcmFrame::new(vec![1i16; 960], 48_000, 1));
        assert!(out.is_empty());
    }

    #[test]
    fn flush_returns_buffered_tail() {
        let mut chunker = FrameChunker::new_20ms();
        // 100 samples: no full 320-window, all buffered.
        let out = chunker.push(&PcmFrame::new(vec![1i16; 100], 16_000, 1));
        assert!(out.is_empty());
        let tail = chunker.flush().expect("tail buffered");
        assert_eq!(tail.data.len(), 100);
        // Second flush is empty.
        assert!(chunker.flush().is_none());
    }

    #[test]
    fn accumulates_across_many_small_frames() {
        let mut chunker = FrameChunker::new_20ms();
        let mut total = 0;
        // Ten 32-sample frames = 320 = exactly one window.
        for _ in 0..10 {
            total += chunker
                .push(&PcmFrame::new(vec![1i16; 32], 16_000, 1))
                .len();
        }
        assert_eq!(total, 1);
    }
}
