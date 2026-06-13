//! xAI TTS adapter — turn Ocean's reply text into call-ready PCM, behind the
//! [`crate::session_task::live::TtsSynth`] trait.
//!
//! The active lane (wake-word → agent turn → speak) needs the agent's reply
//! **text** rendered to **16 kHz mono i16 PCM** so [`crate::speaker`] can pace it
//! into the LiveKit room. The surface already proves the xAI TTS endpoint in
//! production:
//!
//! ```text
//! POST https://api.x.ai/v1/tts
//!   Authorization: Bearer <XAI_API_KEY>
//!   json: { "model":"grok-tts", "text":<reply>, "voice":<profile>,
//!           "language":"en", "response_format":"wav" }
//!   → audio bytes (a WAV container at the model's native rate)
//! ```
//! (verified shape: ocean-surface/crates/ocean-surface-proxy/src/main.rs:1174-1208,
//! which requests `mp3`; the call lane asks for `wav` instead so the decode is
//! dependency-free and the PCM can be resampled to the 16 kHz mono the room tap
//! and STT lane already use.)
//!
//! Why WAV and not mp3: the browser hands mp3 straight to `<audio>` and lets the
//! OS decode it. The call lane has no decoder — it must hand raw i16 PCM to
//! `capture_frame`. Decoding mp3 in-process would pull a heavy native codec;
//! parsing a PCM WAV container is a few bytes of header math, so the whole
//! decode + resample path here is pure and unit-tested without a codec, a
//! network call, or an xAI key. Only the live HTTP POST is gated behind the
//! `xai-tts` feature.

use crate::frame::PcmFrame;

/// Endpoint + model constants, matching the verified surface usage.
pub const XAI_TTS_URL: &str = "https://api.x.ai/v1/tts";
pub const XAI_TTS_MODEL: &str = "grok-tts";
/// xAI's default voice profile (the surface calls it "leo").
pub const XAI_TTS_DEFAULT_VOICE: &str = "leo";

/// The PCM format the call lane speaks in: 16 kHz mono. The room tap delivers
/// audio at this rate and the STT lane buffers it, so the spoken reply matches.
pub const CALL_SAMPLE_RATE: u32 = 16_000;
pub const CALL_CHANNELS: u32 = 1;

/// Build the JSON request body for the xAI TTS endpoint. Kept pure (no client,
/// no key) so the request shape is unit-testable. `voice` selects the profile;
/// `response_format` is fixed to `wav` so [`decode_wav_pcm`] can parse it.
pub fn tts_request_body(text: &str, voice: &str) -> serde_json::Value {
    serde_json::json!({
        "model": XAI_TTS_MODEL,
        "text": text,
        "voice": voice,
        "language": "en",
        "response_format": "wav",
    })
}

/// Decoded PCM pulled out of a WAV container: interleaved i16 samples plus the
/// format needed to interpret and resample them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedPcm {
    /// Interleaved i16 samples, `frames * channels` long.
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u16,
}

/// WAV `fmt ` audio-format tag for integer PCM.
const WAV_FMT_PCM: u16 = 1;
/// WAV `fmt ` audio-format tag for IEEE float PCM.
const WAV_FMT_FLOAT: u16 = 3;
/// WAV `fmt ` audio-format tag for WAVE_FORMAT_EXTENSIBLE; the real format lives
/// in the extension's sub-format GUID, whose first two bytes mirror the tags above.
const WAV_FMT_EXTENSIBLE: u16 = 0xFFFE;

fn read_u16_le(b: &[u8], off: usize) -> anyhow::Result<u16> {
    b.get(off..off + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or_else(|| anyhow::anyhow!("wav: truncated u16 at {off}"))
}

fn read_u32_le(b: &[u8], off: usize) -> anyhow::Result<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| anyhow::anyhow!("wav: truncated u32 at {off}"))
}

/// Parse a 16-bit / 8-bit / 32-bit-float PCM WAV container into interleaved i16
/// samples.
///
/// Walks the RIFF chunk list rather than assuming a fixed 44-byte header, so it
/// tolerates extra chunks (`LIST`, `fact`, …) and the `fmt ` extension some
/// encoders emit, in any order. Float (`audio_format == 3`) samples are clamped
/// and scaled to i16 so the rest of the pipeline only ever sees i16.
///
/// Errors (rather than guessing) on a non-WAV blob, an unsupported bit depth, or
/// a missing required chunk — a malformed TTS response must surface, not play as
/// noise.
pub fn decode_wav_pcm(bytes: &[u8]) -> anyhow::Result<DecodedPcm> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        anyhow::bail!("not a RIFF/WAVE container");
    }

    let mut pos = 12usize;
    let mut audio_format: Option<u16> = None;
    let mut channels: u16 = 0;
    let mut sample_rate: u32 = 0;
    let mut bits_per_sample: u16 = 0;
    let mut data: Option<&[u8]> = None;

    // Walk chunks: each is a 4-byte id, a u32 LE size, then `size` bytes, padded
    // to even length.
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = read_u32_le(bytes, pos + 4)? as usize;
        let body_start = pos + 8;
        let body_end = body_start
            .checked_add(size)
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| anyhow::anyhow!("wav: chunk size {size} overruns buffer"))?;

        match id {
            b"fmt " => {
                let f = &bytes[body_start..body_end];
                let mut fmt = read_u16_le(f, 0)?;
                channels = read_u16_le(f, 2)?;
                sample_rate = read_u32_le(f, 4)?;
                bits_per_sample = read_u16_le(f, 14)?;
                // WAVE_FORMAT_EXTENSIBLE hides the true format tag in the
                // sub-format GUID (first two bytes), starting at byte 24 of fmt.
                if fmt == WAV_FMT_EXTENSIBLE && f.len() >= 26 {
                    fmt = read_u16_le(f, 24)?;
                }
                audio_format = Some(fmt);
            }
            b"data" => {
                data = Some(&bytes[body_start..body_end]);
            }
            _ => {}
        }

        // Advance past body + pad byte (chunks are word-aligned).
        pos = body_end + (size & 1);
    }

    let audio_format = audio_format.ok_or_else(|| anyhow::anyhow!("wav: missing fmt chunk"))?;
    let data = data.ok_or_else(|| anyhow::anyhow!("wav: missing data chunk"))?;
    if channels == 0 || sample_rate == 0 {
        anyhow::bail!("wav: zero channels or sample rate");
    }

    let samples = match (audio_format, bits_per_sample) {
        (WAV_FMT_PCM, 16) => data
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect(),
        (WAV_FMT_PCM, 8) => data
            // 8-bit WAV PCM is unsigned (0..255, 128 = silence) → center to i16.
            .iter()
            .map(|&b| ((b as i32 - 128) * 256) as i16)
            .collect(),
        (WAV_FMT_FLOAT, 32) => data
            .chunks_exact(4)
            .map(|c| {
                let f = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                float_to_i16(f)
            })
            .collect(),
        (fmt, bits) => {
            anyhow::bail!("wav: unsupported format tag {fmt} / {bits}-bit");
        }
    };

    Ok(DecodedPcm {
        samples,
        sample_rate,
        channels,
    })
}

/// Clamp a normalized [-1.0, 1.0] float sample to the i16 range.
fn float_to_i16(f: f32) -> i16 {
    let clamped = f.clamp(-1.0, 1.0);
    // Scale toward i16::MAX; negative side reaches -i16::MAX at -1.0.
    (clamped * i16::MAX as f32).round() as i16
}

/// Downmix interleaved multi-channel i16 PCM to mono by averaging channels.
/// A mono input is returned unchanged. Averaging in i32 avoids overflow.
pub fn to_mono(samples: &[i16], channels: u16) -> Vec<i16> {
    let ch = channels.max(1) as usize;
    if ch == 1 {
        return samples.to_vec();
    }
    samples
        .chunks_exact(ch)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|&s| s as i32).sum();
            (sum / ch as i32) as i16
        })
        .collect()
}

/// Linearly resample mono i16 PCM from `src_rate` to `dst_rate`.
///
/// Linear interpolation is intentionally simple — it is cheap, allocation-light,
/// and good enough for speech where the source is already band-limited by the
/// TTS model. It is also fully deterministic, so the resample math is unit-tested
/// directly. A no-op fast path returns the input when the rates already match.
pub fn resample_linear(samples: &[i16], src_rate: u32, dst_rate: u32) -> Vec<i16> {
    if src_rate == 0 || dst_rate == 0 || src_rate == dst_rate || samples.is_empty() {
        return samples.to_vec();
    }
    let src_len = samples.len();
    if src_len == 1 {
        return samples.to_vec();
    }

    // Output length scales by the rate ratio (round to nearest frame).
    let dst_len = (((src_len as u64) * (dst_rate as u64) + (src_rate as u64) / 2)
        / (src_rate as u64))
        .max(1) as usize;

    let mut out = Vec::with_capacity(dst_len);
    // Map each output index back to a fractional source position. Using
    // (src_len-1)/(dst_len-1) keeps the endpoints aligned (no edge drift).
    let step = (src_len - 1) as f64 / (dst_len.max(2) - 1) as f64;
    for i in 0..dst_len {
        let src_pos = i as f64 * step;
        let idx = src_pos.floor() as usize;
        let frac = src_pos - idx as f64;
        let a = samples[idx] as f64;
        let b = samples[(idx + 1).min(src_len - 1)] as f64;
        let v = a + (b - a) * frac;
        out.push(v.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
    }
    out
}

/// Decode a TTS WAV blob and convert it to the call's 16 kHz mono PCM frame:
/// parse → downmix to mono → resample to 16 kHz. This is the whole pure pipeline
/// the live synth runs on the HTTP response; testing it needs only bytes in.
pub fn wav_to_call_pcm(bytes: &[u8]) -> anyhow::Result<PcmFrame> {
    let decoded = decode_wav_pcm(bytes)?;
    let mono = to_mono(&decoded.samples, decoded.channels);
    let resampled = resample_linear(&mono, decoded.sample_rate, CALL_SAMPLE_RATE);
    Ok(PcmFrame::new(resampled, CALL_SAMPLE_RATE, CALL_CHANNELS))
}

// The live HTTP synthesis is gated behind `xai-tts` (needs a runtime key +
// reqwest). The decode/resample/request-shaping logic above is always built.
#[cfg(feature = "xai-tts")]
pub mod live {
    use super::*;
    use crate::session_task::live::TtsSynth;

    /// [`TtsSynth`] backed by the xAI TTS endpoint. Each reply is POSTed as JSON;
    /// the WAV response is decoded and resampled to 16 kHz mono PCM. Needs
    /// `XAI_API_KEY` and the `xai-tts` feature.
    pub struct XaiTtsSynth {
        client: reqwest::Client,
        api_key: String,
        voice: String,
    }

    impl XaiTtsSynth {
        /// Synthesize with xAI's default voice profile.
        pub fn new(api_key: impl Into<String>) -> Self {
            Self::with_voice(api_key, XAI_TTS_DEFAULT_VOICE)
        }

        /// Synthesize with an explicit voice profile.
        pub fn with_voice(api_key: impl Into<String>, voice: impl Into<String>) -> Self {
            Self {
                client: reqwest::Client::new(),
                api_key: api_key.into(),
                voice: voice.into(),
            }
        }
    }

    #[async_trait::async_trait]
    impl TtsSynth for XaiTtsSynth {
        async fn synth(&mut self, text: &str) -> anyhow::Result<PcmFrame> {
            let resp = self
                .client
                .post(XAI_TTS_URL)
                .bearer_auth(&self.api_key)
                .json(&tts_request_body(text, &self.voice))
                .send()
                .await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let detail = resp.text().await.unwrap_or_default();
                anyhow::bail!("xai tts http {status}: {detail}");
            }
            let bytes = resp.bytes().await?;
            wav_to_call_pcm(&bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Re-encode i16 PCM as a minimal 16-bit WAV so the decoder has real input.
    /// Mirrors [`crate::stt_xai::encode_wav`] (kept local to avoid coupling the
    /// TTS tests to the STT module's encoder).
    fn wav16(samples: &[i16], sample_rate: u32, channels: u16) -> Vec<u8> {
        let byte_rate = sample_rate * channels as u32 * 2;
        let block_align = channels * 2;
        let data_len = (samples.len() * 2) as u32;
        let mut out = Vec::with_capacity(44 + samples.len() * 2);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    /// Encode mono f32 PCM as a 32-bit IEEE-float WAV (audio_format = 3).
    fn wav_f32(samples: &[f32], sample_rate: u32) -> Vec<u8> {
        let channels = 1u16;
        let byte_rate = sample_rate * 4;
        let data_len = (samples.len() * 4) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&WAV_FMT_FLOAT.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&(channels * 4).to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    #[test]
    fn request_body_shapes_the_xai_tts_call() {
        let body = tts_request_body("hello there", "leo");
        assert_eq!(body["model"], XAI_TTS_MODEL);
        assert_eq!(body["text"], "hello there");
        assert_eq!(body["voice"], "leo");
        assert_eq!(body["language"], "en");
        // Must be wav so the decoder can parse it (the surface uses mp3; the call
        // lane diverges here on purpose).
        assert_eq!(body["response_format"], "wav");
    }

    #[test]
    fn decodes_16bit_pcm_wav_roundtrip() {
        let pcm = vec![0i16, 1, -1, 1000, -1000, i16::MAX, i16::MIN];
        let wav = wav16(&pcm, 16_000, 1);
        let decoded = decode_wav_pcm(&wav).unwrap();
        assert_eq!(decoded.sample_rate, 16_000);
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.samples, pcm);
    }

    #[test]
    fn decodes_float32_wav_to_i16() {
        let pcm = vec![0.0f32, 1.0, -1.0, 0.5, -0.5];
        let wav = wav_f32(&pcm, 24_000);
        let decoded = decode_wav_pcm(&wav).unwrap();
        assert_eq!(decoded.sample_rate, 24_000);
        assert_eq!(decoded.channels, 1);
        // 1.0 → i16::MAX, -1.0 → -i16::MAX, 0.5 → ~half.
        assert_eq!(decoded.samples[0], 0);
        assert_eq!(decoded.samples[1], i16::MAX);
        assert_eq!(decoded.samples[2], -i16::MAX);
        assert!((decoded.samples[3] - 16_384).abs() <= 1);
        assert!((decoded.samples[4] + 16_384).abs() <= 1);
    }

    #[test]
    fn decodes_8bit_unsigned_pcm_centered_to_i16() {
        // 8-bit WAV PCM is unsigned: 128 is silence, 0 and 255 are the extremes.
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&0u32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&8_000u32.to_le_bytes());
        wav.extend_from_slice(&8_000u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // block align
        wav.extend_from_slice(&8u16.to_le_bytes()); // 8-bit
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&3u32.to_le_bytes());
        wav.extend_from_slice(&[128, 0, 255]); // silence, min, max
        let decoded = decode_wav_pcm(&wav).unwrap();
        assert_eq!(decoded.samples[0], 0); // 128 → 0
        assert_eq!(decoded.samples[1], -128 * 256); // 0 → -32768
        assert_eq!(decoded.samples[2], 127 * 256); // 255 → 32512
    }

    #[test]
    fn decode_tolerates_extra_chunks_before_data() {
        // Build a WAV with a junk "LIST" chunk between fmt and data; the walker
        // must skip it and still find the PCM.
        let pcm = vec![5i16, 6, 7, 8];
        let mut wav = wav16(&pcm, 16_000, 1);
        // Splice a LIST chunk right after the fmt chunk (fmt ends at byte 36).
        let list = {
            let payload = b"INFOxxxx"; // 8 bytes
            let mut c = Vec::new();
            c.extend_from_slice(b"LIST");
            c.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            c.extend_from_slice(payload);
            c
        };
        wav.splice(36..36, list);
        let decoded = decode_wav_pcm(&wav).unwrap();
        assert_eq!(decoded.samples, pcm);
    }

    #[test]
    fn decode_rejects_non_wav() {
        assert!(decode_wav_pcm(b"not audio at all").is_err());
        assert!(decode_wav_pcm(&[]).is_err());
        // RIFF but not WAVE.
        let mut bogus = Vec::new();
        bogus.extend_from_slice(b"RIFF");
        bogus.extend_from_slice(&0u32.to_le_bytes());
        bogus.extend_from_slice(b"AVI ");
        assert!(decode_wav_pcm(&bogus).is_err());
    }

    #[test]
    fn decode_rejects_unsupported_bit_depth() {
        // Hand-build a 24-bit PCM fmt chunk → unsupported, must error not guess.
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&0u32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&48_000u32.to_le_bytes());
        wav.extend_from_slice(&3u16.to_le_bytes());
        wav.extend_from_slice(&24u16.to_le_bytes()); // 24-bit
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&3u32.to_le_bytes());
        wav.extend_from_slice(&[0, 0, 0]);
        assert!(decode_wav_pcm(&wav).is_err());
    }

    #[test]
    fn decode_handles_extensible_fmt_tag() {
        // WAVE_FORMAT_EXTENSIBLE (0xFFFE) with a 16-bit-PCM sub-format GUID. The
        // decoder must read the real tag from the GUID's first two bytes.
        let pcm = vec![11i16, -22, 33];
        let data_len = (pcm.len() * 2) as u32;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&0u32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&40u32.to_le_bytes()); // extensible fmt is 40 bytes
        wav.extend_from_slice(&WAV_FMT_EXTENSIBLE.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&32_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // 16-bit
        wav.extend_from_slice(&22u16.to_le_bytes()); // cbSize
        wav.extend_from_slice(&16u16.to_le_bytes()); // valid bits per sample
        wav.extend_from_slice(&0u32.to_le_bytes()); // channel mask
                                                    // sub-format GUID: first two bytes = real format tag (1 = PCM).
        wav.extend_from_slice(&WAV_FMT_PCM.to_le_bytes());
        wav.extend_from_slice(&[0u8; 14]); // rest of the GUID
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for s in &pcm {
            wav.extend_from_slice(&s.to_le_bytes());
        }
        let decoded = decode_wav_pcm(&wav).unwrap();
        assert_eq!(decoded.samples, pcm);
        assert_eq!(decoded.sample_rate, 16_000);
    }

    #[test]
    fn mono_downmix_averages_stereo() {
        // Stereo interleaved: L,R,L,R. Average per frame.
        let stereo = vec![100i16, 200, -100, 100, 0, 0];
        let mono = to_mono(&stereo, 2);
        assert_eq!(mono, vec![150, 0, 0]);
    }

    #[test]
    fn mono_passthrough_when_already_mono() {
        let m = vec![1i16, 2, 3];
        assert_eq!(to_mono(&m, 1), m);
    }

    #[test]
    fn resample_is_noop_when_rates_match() {
        let s = vec![1i16, 2, 3, 4];
        assert_eq!(resample_linear(&s, 16_000, 16_000), s);
    }

    #[test]
    fn resample_downsample_halves_length() {
        // 48kHz → 16kHz is a 3:1 decimation; 9 samples → ~3.
        let s: Vec<i16> = (0..9).collect();
        let out = resample_linear(&s, 48_000, 16_000);
        assert_eq!(out.len(), 3);
        // Endpoints preserved by the (n-1)/(m-1) mapping.
        assert_eq!(*out.first().unwrap(), 0);
        assert_eq!(*out.last().unwrap(), 8);
    }

    #[test]
    fn resample_upsample_grows_length_and_interpolates() {
        // 8kHz → 16kHz doubles the rate, so 3 input samples become 6
        // (round(3 * 16000 / 8000)). The (n-1)/(m-1) index map pins the
        // endpoints, so a linear ramp stays a monotonic ramp with interpolated
        // midpoints: [0,10,20] → [0,4,8,12,16,20].
        let s = vec![0i16, 10, 20];
        let out = resample_linear(&s, 8_000, 16_000);
        assert_eq!(out.len(), 6);
        assert_eq!(*out.first().unwrap(), 0);
        assert_eq!(*out.last().unwrap(), 20);
        // Interior is non-decreasing (interpolated ramp).
        for w in out.windows(2) {
            assert!(w[1] >= w[0], "ramp should be monotonic: {out:?}");
        }
    }

    #[test]
    fn resample_handles_degenerate_inputs() {
        assert!(resample_linear(&[], 48_000, 16_000).is_empty());
        assert_eq!(resample_linear(&[7], 48_000, 16_000), vec![7]);
        // Zero rate is a no-op (can't divide), never a panic.
        assert_eq!(resample_linear(&[1, 2], 0, 16_000), vec![1, 2]);
    }

    #[test]
    fn full_pipeline_stereo_48k_to_mono_16k_frame() {
        // A stereo 48kHz WAV → the 16kHz mono PcmFrame the speaker expects.
        // 480 stereo frames (960 interleaved) @48k = 10ms → ~160 samples @16k.
        let interleaved: Vec<i16> = (0..960).map(|i| (i % 100) as i16).collect();
        let wav = wav16(&interleaved, 48_000, 2);
        let frame = wav_to_call_pcm(&wav).unwrap();
        assert_eq!(frame.sample_rate, CALL_SAMPLE_RATE);
        assert_eq!(frame.num_channels, CALL_CHANNELS);
        // 480 per-channel frames at 48k → 160 at 16k (3:1).
        assert_eq!(frame.samples_per_channel, 160);
        assert_eq!(frame.data.len(), 160);
    }

    #[test]
    fn full_pipeline_native_16k_mono_is_lossless_passthrough() {
        // A 16kHz mono WAV (the model's ideal output) survives byte-for-byte:
        // no downmix, no resample.
        let pcm: Vec<i16> = (0..320).map(|i| (i as i16) - 160).collect();
        let wav = wav16(&pcm, 16_000, 1);
        let frame = wav_to_call_pcm(&wav).unwrap();
        assert_eq!(frame.sample_rate, 16_000);
        assert_eq!(frame.num_channels, 1);
        assert_eq!(frame.data, pcm);
    }

    #[test]
    fn full_pipeline_rejects_malformed_response() {
        // A non-WAV TTS response must error out of the whole pipeline, never play
        // as noise.
        assert!(wav_to_call_pcm(b"<html>error</html>").is_err());
    }
}
