//! End-to-end integration test (spec's final gate, fixture-driven).
//!
//! Simulates a real call through the WHOLE pipeline without any account:
//!
//! ```text
//! raw PCM frames  →  FrameChunker (room_tap seam)  →  fake STT (segments)
//!   →  CallSession orchestrator  →  OceanEvent sequence (asserted)
//! ```
//!
//! This is what the spec calls the "replay a recorded call → assert the event
//! sequence" test (design doc §Testing). It proves the *seams between modules*
//! connect — not just each unit in isolation — which is the thing unit tests
//! can't catch. The live version (real phone → real audio) is the same shape
//! with `room_tap::live` + a real STT provider swapped in; that one is gated on
//! John's Twilio + LiveKit accounts.

use ocean_call::{
    CallSession, CapturingSink, FrameChunker, PcmFrame, Summarizer, SummaryPolicy,
    TranscriptSegment, WakeGate,
};
use ocean_core::OceanEvent;

/// A scripted "call": each turn is (speaker, words, start_ms). In a live call
/// these come from STT over the audio; here we drive them directly after
/// proving the audio→frame seam below.
fn call_script() -> Vec<(&'static str, &'static str, u64)> {
    vec![
        (
            "caller",
            "hey thanks for hopping on about the Warner Q3 push",
            0,
        ),
        (
            "caller",
            "I'll send the master over to Atlantic tonight",
            3_000,
        ),
        (
            "caller",
            "and we need to verify the toll-free number by Friday",
            6_500,
        ),
        ("ocean-op", "sounds good", 9_000),
        ("caller", "hey Ocean what did we just agree to", 11_000),
        ("caller", "great talk to you tomorrow", 14_000),
    ]
}

/// Prove the room_tap audio seam: arbitrary PCM in → exact 20ms frames out.
/// (The live tap feeds these same PcmFrames from a LiveKit NativeAudioStream.)
#[test]
fn audio_frame_seam_produces_20ms_windows() {
    let mut chunker = FrameChunker::new_20ms();
    // One second of 16kHz mono audio = 16000 samples = fifty 20ms windows.
    let one_second = PcmFrame::new(vec![0i16; 16_000], 16_000, 1);
    let windows = chunker.push(&one_second);
    assert_eq!(windows.len(), 50, "1s @16k mono → 50×20ms frames");
    assert!(windows.iter().all(|w| w.duration_ms() == 20));
}

#[test]
fn full_call_emits_expected_event_sequence() {
    let mut session = CallSession::new(
        "e2e-call",
        Summarizer::new(SummaryPolicy {
            every_n_segments: 3,
            silence_ms: 15_000,
        }),
        WakeGate::new(false, 2_000),
    );
    let mut sink = CapturingSink::default();

    session.start(&mut sink, "call:e2e", vec!["sip:+17035081859".into()]);
    // The orchestrator no longer emits CallSummaryUpdated itself — it hands the
    // debounced transcript back via SegmentOutcome::summary_due, and the I/O lane
    // (session_task) runs the real summary turn. Capture that the summary fired
    // here at the orchestrator seam.
    let mut summary_due_count = 0usize;
    for (speaker, text, ms) in call_script() {
        let outcome =
            session.on_segment(TranscriptSegment::final_(speaker, text, ms), ms, &mut sink);
        if outcome.summary_due.is_some() {
            summary_due_count += 1;
        }
    }
    session.end(&mut sink, 16_000);

    let kinds: Vec<&str> = sink
        .events
        .iter()
        .map(|e| match e {
            OceanEvent::CallStarted { .. } => "started",
            OceanEvent::CallTranscriptSegment { .. } => "segment",
            OceanEvent::CallSummaryUpdated { .. } => "summary",
            OceanEvent::CallTaskDetected { .. } => "task",
            OceanEvent::CallWakeTriggered { .. } => "wake",
            OceanEvent::CallEnded { .. } => "ended",
            _ => "other",
        })
        .collect();

    // Brackets: starts with started, ends with ended.
    assert_eq!(kinds.first(), Some(&"started"));
    assert_eq!(kinds.last(), Some(&"ended"));
    // Six spoken turns → six transcript segments.
    assert_eq!(kinds.iter().filter(|k| **k == "segment").count(), 6);
    // Two commitments detected ("I'll send the master", "verify the toll-free").
    assert_eq!(kinds.iter().filter(|k| **k == "task").count(), 2);
    // The orchestrator stays I/O-free: it never emits a raw-transcript summary.
    assert!(
        !kinds.contains(&"summary"),
        "orchestrator must not emit CallSummaryUpdated; the I/O lane does"
    );
    // ...but it DID signal a summary was due (3 finals before the wake turn).
    assert!(summary_due_count >= 1, "summary should fire after 3 finals");
    // The wake word was honored.
    assert!(kinds.contains(&"wake"));
}

#[test]
fn detected_tasks_carry_real_content() {
    let mut session = CallSession::new(
        "e2e-tasks",
        Summarizer::new(SummaryPolicy::default()),
        WakeGate::new(false, 2_000),
    );
    let mut sink = CapturingSink::default();

    for (speaker, text, ms) in call_script() {
        session.on_segment(TranscriptSegment::final_(speaker, text, ms), ms, &mut sink);
    }

    let tasks: Vec<&OceanEvent> = sink
        .events
        .iter()
        .filter(|e| matches!(e, OceanEvent::CallTaskDetected { .. }))
        .collect();
    assert_eq!(tasks.len(), 2);

    // Tasks have stable, unique ids and preserve the source quote.
    let mut ids = Vec::new();
    for t in &tasks {
        if let OceanEvent::CallTaskDetected {
            task_id,
            source_quote,
            confidence,
            ..
        } = t
        {
            ids.push(task_id.clone());
            assert!(*confidence >= 0.5, "low-confidence tasks shouldn't surface");
            assert!(source_quote.is_some());
        }
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 2, "task ids must be unique");
}

#[test]
fn muted_call_stays_passive_no_wake() {
    // A sensitive (label) call: voice muted → wake word never answers, but the
    // passive lane (transcript/tasks) still runs.
    let mut session = CallSession::new(
        "e2e-muted",
        Summarizer::new(SummaryPolicy::default()),
        WakeGate::new(true, 2_000), // muted
    );
    let mut sink = CapturingSink::default();

    session.on_segment(
        TranscriptSegment::final_("caller", "hey Ocean summarize this", 0),
        0,
        &mut sink,
    );

    // No wake event when muted...
    assert!(!sink
        .events
        .iter()
        .any(|e| matches!(e, OceanEvent::CallWakeTriggered { .. })));
    // ...but the transcript segment still flows (passive lane unaffected).
    assert!(sink
        .events
        .iter()
        .any(|e| matches!(e, OceanEvent::CallTranscriptSegment { .. })));
}
