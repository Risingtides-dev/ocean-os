//! xAI STT adapter (batch endpoint, verified) behind the [`SttProvider`] trait.
//!
//! The only xAI STT surface confirmed in real code is the **batch** endpoint
//! that ocean-surface's proxy already uses in production:
//!
//! ```text
//! POST https://api.x.ai/v1/stt
//!   Authorization: Bearer <XAI_API_KEY>
//!   multipart: file=<audio bytes>, model="grok-stt", response_format="json"
//!   → { "text": "<transcript>" }
//! ```
//! (verified: ocean-surface/crates/ocean-surface-proxy/src/main.rs:692-723)
//!
//! A streaming `wss://api.x.ai/v1/stt` was mentioned in third-party research
//! but is NOT verified against code or official docs, so this adapter does not
//! assume it. Instead it satisfies [`SttProvider`] by **buffering utterances**:
//! frames accumulate, and a [`flush`](XaiSttProvider::flush)-driven utterance is
//! POSTed to the batch endpoint, yielding one final [`TranscriptSegment`]. A
//! true streaming provider can replace this behind the same trait without
//! touching the orchestrator — that's the point of the trait.
//!
//! The request-building + WAV-encoding logic is unit-tested here; the actual
//! HTTP call is exercised only with a live key (gated behind `xai-stt`).

use crate::frame::PcmFrame;

/// Endpoint + model constants, matching the verified proxy usage.
pub const XAI_STT_URL: &str = "https://api.x.ai/v1/stt";
pub const XAI_STT_MODEL: &str = "grok-stt";

/// Encode interleaved i16 PCM as a minimal 16-bit WAV container so the batch
/// endpoint gets a self-describing file (sample rate + channels in the header)
/// rather than raw headerless samples.
pub fn encode_wav(samples: &[i16], sample_rate: u32, num_channels: u16) -> Vec<u8> {
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * num_channels as u32 * (bits_per_sample / 8) as u32;
    let block_align = num_channels * (bits_per_sample / 8);
    let data_len = (samples.len() * 2) as u32;
    let riff_len = 36 + data_len;

    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_len.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    // fmt chunk
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
    out.extend_from_slice(&num_channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    // data chunk
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Buffers PCM frames into one utterance, ready to transcribe on flush.
#[derive(Debug, Default)]
pub struct UtteranceBuffer {
    samples: Vec<i16>,
    sample_rate: u32,
    num_channels: u16,
    /// Call-relative ms of the first buffered frame.
    start_ms: u64,
    has_start: bool,
}

impl UtteranceBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a frame's samples; records the utterance start time once.
    pub fn push(&mut self, frame: &PcmFrame, now_ms: u64) {
        if !self.has_start {
            self.start_ms = now_ms;
            self.sample_rate = frame.sample_rate;
            self.num_channels = frame.num_channels as u16;
            self.has_start = true;
        }
        self.samples.extend_from_slice(&frame.data);
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn start_ms(&self) -> u64 {
        self.start_ms
    }

    /// Take the buffered audio as a WAV blob and reset, returning None if empty.
    pub fn take_wav(&mut self) -> Option<(Vec<u8>, u64)> {
        if self.samples.is_empty() {
            return None;
        }
        let wav = encode_wav(&self.samples, self.sample_rate, self.num_channels.max(1));
        let start = self.start_ms;
        let samples = std::mem::take(&mut self.samples);
        let _ = samples;
        self.has_start = false;
        Some((wav, start))
    }
}

// The live HTTP transcription is gated behind `xai-stt` (needs a runtime key).
#[cfg(feature = "xai-stt")]
pub mod live {
    use super::*;
    use crate::stt::TranscriptSegment;

    /// Transcribe one WAV blob via the verified batch endpoint. Returns the
    /// final transcript segment (speaker is "caller" — batch STT isn't diarized
    /// here; room_tap can label by participant later).
    pub async fn transcribe_wav(
        client: &reqwest::Client,
        api_key: &str,
        wav: Vec<u8>,
        start_ms: u64,
    ) -> anyhow::Result<Option<TranscriptSegment>> {
        let part = reqwest::multipart::Part::bytes(wav)
            .file_name("utterance.wav")
            .mime_str("audio/wav")?;
        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", XAI_STT_MODEL)
            .text("response_format", "json");

        let resp = client
            .post(XAI_STT_URL)
            .bearer_auth(api_key)
            .multipart(form)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("xai stt http {}", resp.status());
        }
        let body: serde_json::Value = resp.json().await?;
        let text = body
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if text.is_empty() {
            return Ok(None);
        }
        Ok(Some(TranscriptSegment::final_("caller", text, start_ms)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_well_formed() {
        let wav = encode_wav(&[0, 1, -1, 100], 16_000, 1);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        // 44-byte header + 4 samples * 2 bytes.
        assert_eq!(wav.len(), 44 + 8);
    }

    #[test]
    fn wav_encodes_sample_rate_in_header() {
        let wav = encode_wav(&[0; 320], 16_000, 1);
        // sample rate is little-endian u32 at offset 24.
        let sr = u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]);
        assert_eq!(sr, 16_000);
    }

    #[test]
    fn buffer_accumulates_and_records_start() {
        let mut buf = UtteranceBuffer::new();
        assert!(buf.is_empty());
        buf.push(&PcmFrame::new(vec![1, 2, 3], 16_000, 1), 500);
        buf.push(&PcmFrame::new(vec![4, 5, 6], 16_000, 1), 520);
        assert!(!buf.is_empty());
        assert_eq!(buf.start_ms(), 500); // first frame's time
    }

    #[test]
    fn take_wav_resets_buffer() {
        let mut buf = UtteranceBuffer::new();
        buf.push(&PcmFrame::new(vec![1; 320], 16_000, 1), 0);
        let (wav, start) = buf.take_wav().unwrap();
        assert_eq!(start, 0);
        assert!(wav.starts_with(b"RIFF"));
        // Drained.
        assert!(buf.is_empty());
        assert!(buf.take_wav().is_none());
    }
}
