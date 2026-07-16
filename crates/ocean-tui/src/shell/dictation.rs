//! Native microphone capture for composer dictation.
//!
//! The TUI owns capture and metering only. Provider credentials and speech
//! recognition stay in the daemon behind `POST /v1/voice/stt`.

use std::sync::mpsc;

use tokio::sync::mpsc::UnboundedSender;

use super::action::Action;

#[cfg(target_os = "macos")]
const OUTPUT_SAMPLE_RATE: u32 = 16_000;
#[cfg(target_os = "macos")]
const MAX_CAPTURE_SECS: u64 = 30;
#[cfg(target_os = "macos")]
const MIN_CAPTURE_MS: u64 = 120;

#[derive(Clone, Copy)]
enum CaptureCommand {
    Finish,
    Cancel,
}

/// An owned capture lifetime. Dropping it always asks the capture thread to
/// discard the microphone stream, so leaving the TUI cannot strand a hot mic.
pub struct CaptureHandle {
    commands: mpsc::Sender<CaptureCommand>,
    closed: bool,
}

impl CaptureHandle {
    pub fn finish(&mut self) {
        if !self.closed {
            let _ = self.commands.send(CaptureCommand::Finish);
            self.closed = true;
        }
    }

    pub fn cancel(&mut self) {
        if !self.closed {
            let _ = self.commands.send(CaptureCommand::Cancel);
            self.closed = true;
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Start one bounded capture. Device setup happens on a dedicated thread; all
/// results return through Elm actions and never mutate the app from callbacks.
pub fn start(id: u64, actions: UnboundedSender<Action>) -> Result<CaptureHandle, String> {
    let (commands, rx) = mpsc::channel();
    let commands_on_error = commands.clone();
    std::thread::Builder::new()
        .name("ocean-dictation".into())
        .spawn(move || capture(id, actions, rx, commands_on_error))
        .map_err(|error| format!("could not start microphone capture: {error}"))?;
    Ok(CaptureHandle {
        commands,
        closed: false,
    })
}

#[cfg(not(target_os = "macos"))]
fn capture(
    id: u64,
    actions: UnboundedSender<Action>,
    _commands: mpsc::Receiver<CaptureCommand>,
    _commands_on_error: mpsc::Sender<CaptureCommand>,
) {
    let _ = actions.send(Action::DictationCaptured {
        id,
        audio: Err("dictation capture is currently supported on macOS".into()),
    });
}

#[cfg(target_os = "macos")]
fn capture(
    id: u64,
    actions: UnboundedSender<Action>,
    commands: mpsc::Receiver<CaptureCommand>,
    commands_on_error: mpsc::Sender<CaptureCommand>,
) {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let result = (|| -> Result<(), String> {
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or_else(|| {
            "no microphone input found; check System Settings → Privacy & Security → Microphone"
                .to_string()
        })?;
        let supported = device
            .default_input_config()
            .map_err(|error| microphone_error("microphone unavailable", error))?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let sample_rate = config.sample_rate.0;
        let channels = usize::from(config.channels.max(1));
        let max_samples = (sample_rate as usize).saturating_mul(MAX_CAPTURE_SECS as usize);
        let samples = Arc::new(Mutex::new(Vec::<i16>::with_capacity(max_samples)));
        let stream_error = Arc::new(Mutex::new(None::<String>));
        let error_slot = Arc::clone(&stream_error);
        let error_callback = move |error| {
            if let Ok(mut slot) = error_slot.lock() {
                *slot = Some(microphone_error("microphone stream failed", error));
            }
            let _ = commands_on_error.send(CaptureCommand::Finish);
        };

        let sink = CaptureSink {
            id,
            samples: Arc::clone(&samples),
            actions: actions.clone(),
            meter_at: Arc::new(Mutex::new(Instant::now())),
            max_samples,
        };
        let stream = build_stream(
            &device,
            &config,
            sample_format,
            channels,
            sink,
            error_callback,
        )?;
        // Device discovery/build may outlive a short hold. Honor the queued
        // release/cancel before `play()` so a late-open task never turns on the
        // microphone after the operator has already let go.
        match commands.try_recv() {
            Ok(CaptureCommand::Cancel) | Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            Ok(CaptureCommand::Finish) => {
                return Err("recording ended before the microphone opened — try again".into());
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        stream
            .play()
            .map_err(|error| microphone_error("microphone permission denied", error))?;
        let _ = actions.send(Action::DictationCaptureStarted { id });

        let started = Instant::now();
        let finish = loop {
            match commands.recv_timeout(Duration::from_millis(25)) {
                Ok(CaptureCommand::Finish) => break true,
                Ok(CaptureCommand::Cancel) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break false;
                }
                Err(mpsc::RecvTimeoutError::Timeout)
                    if started.elapsed() >= Duration::from_secs(MAX_CAPTURE_SECS) =>
                {
                    break true;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        };
        drop(stream);
        if !finish {
            return Ok(());
        }
        if let Some(error) = stream_error.lock().ok().and_then(|mut slot| slot.take()) {
            return Err(error);
        }
        if started.elapsed() < Duration::from_millis(MIN_CAPTURE_MS) {
            return Err("recording too short — speak a little longer".into());
        }

        let input = samples
            .lock()
            .map_err(|_| "microphone sample buffer failed".to_string())?;
        let pcm = resample_mono(&input, sample_rate, OUTPUT_SAMPLE_RATE);
        if pcm.is_empty() {
            return Err("no microphone audio captured".into());
        }
        let wav = pcm16_wav(&pcm, OUTPUT_SAMPLE_RATE)?;
        let _ = actions.send(Action::DictationCaptured { id, audio: Ok(wav) });
        Ok(())
    })();

    if let Err(error) = result {
        let _ = actions.send(Action::DictationCaptured {
            id,
            audio: Err(error),
        });
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone)]
struct CaptureSink {
    id: u64,
    samples: std::sync::Arc<std::sync::Mutex<Vec<i16>>>,
    actions: UnboundedSender<Action>,
    meter_at: std::sync::Arc<std::sync::Mutex<std::time::Instant>>,
    max_samples: usize,
}

#[cfg(target_os = "macos")]
fn build_stream<E>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    channels: usize,
    sink: CaptureSink,
    error: E,
) -> Result<cpal::Stream, String>
where
    E: FnMut(cpal::StreamError) + Send + 'static,
{
    use cpal::traits::DeviceTrait;
    let stream = match format {
        cpal::SampleFormat::F32 => {
            let sink = sink.clone();
            device.build_input_stream(
                config,
                move |data: &[f32], _| {
                    ingest(
                        data.chunks(channels).map(|frame| {
                            frame.iter().copied().sum::<f32>() / frame.len().max(1) as f32
                        }),
                        &sink,
                    );
                },
                error,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let sink = sink.clone();
            device.build_input_stream(
                config,
                move |data: &[i16], _| {
                    ingest(
                        data.chunks(channels).map(|frame| {
                            frame.iter().map(|sample| f32::from(*sample)).sum::<f32>()
                                / frame.len().max(1) as f32
                                / f32::from(i16::MAX)
                        }),
                        &sink,
                    );
                },
                error,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let sink = sink.clone();
            device.build_input_stream(
                config,
                move |data: &[u16], _| {
                    ingest(
                        data.chunks(channels).map(|frame| {
                            let average =
                                frame.iter().map(|sample| f32::from(*sample)).sum::<f32>()
                                    / frame.len().max(1) as f32;
                            (average - 32_768.0) / 32_768.0
                        }),
                        &sink,
                    );
                },
                error,
                None,
            )
        }
        other => return Err(format!("unsupported microphone sample format: {other:?}")),
    };
    stream.map_err(|error| microphone_error("could not open microphone", error))
}

#[cfg(target_os = "macos")]
fn ingest<I>(frames: I, sink: &CaptureSink)
where
    I: Iterator<Item = f32>,
{
    use std::time::{Duration, Instant};

    let mut sum_sq = 0.0f32;
    let mut count = 0usize;
    if let Ok(mut output) = sink.samples.lock() {
        for sample in frames {
            if output.len() >= sink.max_samples {
                break;
            }
            let sample = sample.clamp(-1.0, 1.0);
            output.push((sample * f32::from(i16::MAX)) as i16);
            sum_sq += sample * sample;
            count += 1;
        }
    }
    if count == 0 {
        return;
    }
    if let Ok(mut last) = sink.meter_at.lock() {
        if last.elapsed() >= Duration::from_millis(33) {
            *last = Instant::now();
            let rms = (sum_sq / count as f32).sqrt();
            let _ = sink.actions.send(Action::DictationLevel {
                id: sink.id,
                level: normalized_level(rms),
            });
        }
    }
}

#[cfg(target_os = "macos")]
fn microphone_error(context: &str, error: impl std::fmt::Display) -> String {
    format!("{context}: {error}; check System Settings → Privacy & Security → Microphone")
}

#[cfg(any(target_os = "macos", test))]
fn normalized_level(rms: f32) -> f32 {
    if !rms.is_finite() || rms <= 0.0 {
        return 0.0;
    }
    // Map -50dB..0dB to 0..1. Speech commonly falls around -35dB..-12dB.
    ((20.0 * rms.log10() + 50.0) / 50.0).clamp(0.0, 1.0)
}

#[cfg(any(target_os = "macos", test))]
fn resample_mono(samples: &[i16], input_rate: u32, output_rate: u32) -> Vec<i16> {
    if samples.is_empty() || input_rate == 0 || output_rate == 0 {
        return Vec::new();
    }
    if input_rate == output_rate {
        return samples.to_vec();
    }
    let output_len = samples.len().saturating_mul(output_rate as usize) / input_rate as usize;
    (0..output_len)
        .map(|index| {
            let position = index as f64 * input_rate as f64 / output_rate as f64;
            let left = position.floor() as usize;
            let right = (left + 1).min(samples.len() - 1);
            let fraction = position - left as f64;
            (f64::from(samples[left])
                + (f64::from(samples[right]) - f64::from(samples[left])) * fraction)
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
        })
        .collect()
}

#[cfg(any(target_os = "macos", test))]
fn pcm16_wav(samples: &[i16], sample_rate: u32) -> Result<Vec<u8>, String> {
    let data_len = samples
        .len()
        .checked_mul(2)
        .and_then(|len| u32::try_from(len).ok())
        .ok_or_else(|| "dictation recording is too large".to_string())?;
    let riff_len = 36u32
        .checked_add(data_len)
        .ok_or_else(|| "dictation recording is too large".to_string())?;
    let byte_rate = sample_rate
        .checked_mul(2)
        .ok_or_else(|| "invalid dictation sample rate".to_string())?;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_len.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    Ok(wav)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_normalization_is_bounded_and_monotonic() {
        assert_eq!(normalized_level(0.0), 0.0);
        let quiet = normalized_level(0.01);
        let speech = normalized_level(0.1);
        assert!(quiet < speech);
        assert!((0.0..=1.0).contains(&quiet));
        assert_eq!(normalized_level(1.0), 1.0);
    }

    #[test]
    fn resampler_reduces_48k_to_16k() {
        let input: Vec<i16> = (0..48_000).map(|index| (index % 100) as i16).collect();
        let output = resample_mono(&input, 48_000, 16_000);
        assert_eq!(output.len(), 16_000);
        assert_eq!(output[0], input[0]);
    }

    #[test]
    fn wav_header_describes_mono_pcm_payload() {
        let wav = pcm16_wav(&[1, -2, 3], 16_000).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
        assert_eq!(wav.len(), 50);
    }
}
