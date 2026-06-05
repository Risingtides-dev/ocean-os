//! Call orchestrator — wires the verified pieces into one running call.
//!
//! This is the spine that turns the individual units into a working call:
//!
//! ```text
//! transcript segments (from stt_stream)
//!   ├─ SegmentAssembler   → CallTranscriptSegment events (interim + final)
//!   ├─ Summarizer         → CallSummaryUpdated events (debounced)
//!   ├─ task_detector      → CallTaskDetected events (detect-only)
//!   └─ WakeGate           → (active lane: answer decision; TTS in adapter)
//! ```
//!
//! It owns no I/O: audio capture (room_tap), STT, the agent-turn POST, and TTS
//! live in adapters. The orchestrator consumes already-transcribed
//! [`TranscriptSegment`]s and emits [`ocean_core::OceanEvent`]s through an
//! [`EventSink`] — so the whole call-handling policy is unit-testable with a
//! capturing sink and hand-fed segments, no network, no audio, no account.

use ocean_core::OceanEvent;

use crate::stt::{SegmentAssembler, SegmentUpdate, TranscriptSegment};
use crate::summarizer::{SummaryAction, Summarizer};
use crate::task_detector;
use crate::wake::{WakeAction, WakeGate};

/// Where the orchestrator sends the OceanEvents it produces. The daemon impl
/// forwards to its EventBus `emit()`; tests use a capturing impl.
pub trait EventSink: Send {
    fn emit(&mut self, event: OceanEvent);
}

/// A simple capturing sink, handy for tests and for buffering before flush.
#[derive(Debug, Default)]
pub struct CapturingSink {
    pub events: Vec<OceanEvent>,
}

impl EventSink for CapturingSink {
    fn emit(&mut self, event: OceanEvent) {
        self.events.push(event);
    }
}

/// Drives one call's passive + active lanes over incoming transcript segments.
pub struct CallSession {
    call_id: String,
    assembler: SegmentAssembler,
    summarizer: Summarizer,
    wake: WakeGate,
    /// Monotonic counter for stable detected-task ids.
    next_task_seq: u64,
}

/// What the active lane wants the adapter to do after a segment (the
/// orchestrator decides; the adapter runs the agent turn + TTS).
#[derive(Debug, Clone, PartialEq)]
pub enum ActiveOutcome {
    /// Nothing to say.
    Silent,
    /// Run an ephemeral agent turn over this command and speak the reply.
    Answer(String),
}

impl CallSession {
    pub fn new(call_id: impl Into<String>, summarizer: Summarizer, wake: WakeGate) -> Self {
        Self {
            call_id: call_id.into(),
            assembler: SegmentAssembler::new(),
            summarizer,
            wake,
            next_task_seq: 0,
        }
    }

    /// Emit the CallStarted event. Call once when the room is joined.
    pub fn start(&mut self, sink: &mut impl EventSink, room_id: &str, participants: Vec<String>) {
        sink.emit(OceanEvent::CallStarted {
            call_id: self.call_id.clone(),
            room_id: room_id.to_string(),
            participants,
        });
    }

    /// Process one transcript segment through both lanes, emitting events and
    /// returning what the active lane wants done (if anything).
    ///
    /// `now_ms` is wall-clock for wake cooldown + summary silence.
    pub fn on_segment(
        &mut self,
        seg: TranscriptSegment,
        now_ms: u64,
        sink: &mut impl EventSink,
    ) -> ActiveOutcome {
        // --- Transcript lane: emit interim/final segments ---
        match self.assembler.ingest(seg.clone()) {
            SegmentUpdate::Ignore => {}
            SegmentUpdate::Interim(s) | SegmentUpdate::Final(s) => {
                sink.emit(OceanEvent::CallTranscriptSegment {
                    speaker: s.speaker.clone(),
                    text: s.text.clone(),
                    start_ms: s.start_ms,
                    is_final: s.is_final,
                });
            }
        }

        // Summary + task lanes act on FINAL segments only.
        if seg.is_final && !seg.text.trim().is_empty() {
            // --- Summary lane (debounced) ---
            if let SummaryAction::Summarize(text) = self.summarizer.ingest_final(&seg) {
                sink.emit(OceanEvent::CallSummaryUpdated {
                    summary: text,
                    as_of_ms: seg.start_ms,
                });
            }
            // --- Task lane (detect-only) ---
            if let Some(task) = task_detector::detect(&seg) {
                self.next_task_seq += 1;
                sink.emit(OceanEvent::CallTaskDetected {
                    task_id: format!("{}-task-{}", self.call_id, self.next_task_seq),
                    title: task.title,
                    assignee: None,
                    due: None,
                    source_quote: Some(task.source_quote),
                    confidence: task.confidence,
                });
            }
        }

        // --- Active lane: wake-gated voice ---
        match self.wake.on_utterance(&seg.text, now_ms) {
            WakeAction::Silent => ActiveOutcome::Silent,
            WakeAction::Answer(cmd) => {
                sink.emit(OceanEvent::CallWakeTriggered {
                    utterance: cmd.clone(),
                });
                ActiveOutcome::Answer(cmd)
            }
        }
    }

    /// Periodic tick: fires a debounced summary if the call has gone quiet.
    pub fn on_tick(&mut self, now_ms: u64, sink: &mut impl EventSink) {
        if let SummaryAction::Summarize(text) = self.summarizer.on_tick(now_ms) {
            sink.emit(OceanEvent::CallSummaryUpdated {
                summary: text,
                as_of_ms: now_ms,
            });
        }
    }

    /// Call after the active lane finished speaking, to start the wake cooldown.
    pub fn mark_replied(&mut self, now_ms: u64) {
        self.wake.mark_replied(now_ms);
    }

    /// Emit CallEnded. Call once when the room disconnects.
    pub fn end(&mut self, sink: &mut impl EventSink, duration_ms: u64) {
        sink.emit(OceanEvent::CallEnded {
            call_id: self.call_id.clone(),
            duration_ms,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summarizer::SummaryPolicy;

    fn session() -> CallSession {
        CallSession::new(
            "call-1",
            Summarizer::new(SummaryPolicy {
                every_n_segments: 2,
                silence_ms: 5_000,
            }),
            WakeGate::new(false, 2_000),
        )
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
                OceanEvent::CallEnded { .. } => "ended",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn start_emits_call_started() {
        let mut s = session();
        let mut sink = CapturingSink::default();
        s.start(&mut sink, "call:room", vec!["sip:+1703".into()]);
        assert_eq!(ev_types(&sink), vec!["started"]);
    }

    #[test]
    fn final_segment_emits_transcript() {
        let mut s = session();
        let mut sink = CapturingSink::default();
        s.on_segment(TranscriptSegment::final_("s0", "hello there", 0), 0, &mut sink);
        assert!(ev_types(&sink).contains(&"segment"));
    }

    #[test]
    fn commitment_segment_emits_task() {
        let mut s = session();
        let mut sink = CapturingSink::default();
        s.on_segment(
            TranscriptSegment::final_("s0", "I'll send the master tonight", 100),
            100,
            &mut sink,
        );
        assert!(ev_types(&sink).contains(&"task"));
        // The task carries a stable id + the source quote.
        let task = sink
            .events
            .iter()
            .find(|e| matches!(e, OceanEvent::CallTaskDetected { .. }))
            .unwrap();
        if let OceanEvent::CallTaskDetected { task_id, source_quote, .. } = task {
            assert_eq!(task_id, "call-1-task-1");
            assert!(source_quote.as_ref().unwrap().contains("master"));
        }
    }

    #[test]
    fn summary_fires_after_threshold() {
        let mut s = session(); // every_n_segments = 2
        let mut sink = CapturingSink::default();
        s.on_segment(TranscriptSegment::final_("s0", "first point", 0), 0, &mut sink);
        s.on_segment(TranscriptSegment::final_("s0", "second point", 100), 100, &mut sink);
        assert!(ev_types(&sink).contains(&"summary"));
    }

    #[test]
    fn wake_triggers_answer_outcome_and_event() {
        let mut s = session();
        let mut sink = CapturingSink::default();
        let outcome = s.on_segment(
            TranscriptSegment::final_("s0", "hey Ocean what did we decide", 0),
            0,
            &mut sink,
        );
        assert_eq!(outcome, ActiveOutcome::Answer("what did we decide".into()));
        assert!(ev_types(&sink).contains(&"wake"));
    }

    #[test]
    fn unsummoned_segment_is_silent() {
        let mut s = session();
        let mut sink = CapturingSink::default();
        let outcome = s.on_segment(
            TranscriptSegment::final_("s0", "just chatting about the weather", 0),
            0,
            &mut sink,
        );
        assert_eq!(outcome, ActiveOutcome::Silent);
    }

    #[test]
    fn end_emits_call_ended() {
        let mut s = session();
        let mut sink = CapturingSink::default();
        s.end(&mut sink, 612_000);
        assert!(ev_types(&sink).contains(&"ended"));
    }

    #[test]
    fn full_call_event_sequence() {
        let mut s = session();
        let mut sink = CapturingSink::default();
        s.start(&mut sink, "call:room", vec![]);
        s.on_segment(TranscriptSegment::final_("s0", "let's kick off", 0), 0, &mut sink);
        s.on_segment(
            TranscriptSegment::final_("s0", "I'll follow up with Atlantic", 1_000),
            1_000,
            &mut sink,
        );
        s.end(&mut sink, 5_000);
        let types = ev_types(&sink);
        // Starts with started, ends with ended, has a task in between.
        assert_eq!(types.first(), Some(&"started"));
        assert_eq!(types.last(), Some(&"ended"));
        assert!(types.contains(&"task"));
    }
}
