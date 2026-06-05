//! Rolling call summary — debounce logic for when to fire a summary turn.
//!
//! The passive lane keeps a running summary of the call. Re-summarizing on
//! every transcript segment would hammer the agent loop, so the summarizer
//! **debounces**: it accumulates final transcript segments and only fires a
//! summary turn when enough has been said (`every_n_segments`) or the call has
//! gone quiet for a beat (`silence_ms`). The actual `/v1/agent/turns` POST and
//! the LLM live in the adapter; this module is the pure "should we summarize
//! now, and over what text?" decision, fully unit-testable.

use crate::stt::TranscriptSegment;

/// Debounce policy for summary turns.
#[derive(Debug, Clone)]
pub struct SummaryPolicy {
    /// Fire after this many final segments accumulate since the last summary.
    pub every_n_segments: usize,
    /// Or fire if this much wall-clock silence passes with pending segments.
    pub silence_ms: u64,
}

impl Default for SummaryPolicy {
    fn default() -> Self {
        // Summarize roughly every ~8 utterances, or after 15s of quiet.
        Self {
            every_n_segments: 8,
            silence_ms: 15_000,
        }
    }
}

/// Accumulates final segments and decides when a summary turn is due.
#[derive(Debug)]
pub struct Summarizer {
    policy: SummaryPolicy,
    /// Final-segment texts not yet folded into a summary.
    pending: Vec<String>,
    /// Wall-clock ms of the most recent ingested segment.
    last_segment_ms: u64,
}

/// The summarizer's verdict for the most recent input.
#[derive(Debug, Clone, PartialEq)]
pub enum SummaryAction {
    /// Keep buffering; nothing to do yet.
    Wait,
    /// Fire a summary turn over this joined transcript text.
    Summarize(String),
}

impl Summarizer {
    pub fn new(policy: SummaryPolicy) -> Self {
        Self {
            policy,
            pending: Vec::new(),
            last_segment_ms: 0,
        }
    }

    /// Ingest a final transcript segment. Only finals count toward summary —
    /// interims are revisionary and would double-count. Returns whether a
    /// summary should fire now (segment-count threshold).
    pub fn ingest_final(&mut self, seg: &TranscriptSegment) -> SummaryAction {
        if !seg.is_final || seg.text.trim().is_empty() {
            return SummaryAction::Wait;
        }
        self.pending.push(seg.text.trim().to_string());
        self.last_segment_ms = seg.start_ms;
        if self.pending.len() >= self.policy.every_n_segments {
            return self.flush();
        }
        SummaryAction::Wait
    }

    /// Called on a periodic tick with the current wall-clock ms. Fires a
    /// summary if there is pending text and the silence window has elapsed.
    pub fn on_tick(&mut self, now_ms: u64) -> SummaryAction {
        if self.pending.is_empty() {
            return SummaryAction::Wait;
        }
        if now_ms.saturating_sub(self.last_segment_ms) >= self.policy.silence_ms {
            return self.flush();
        }
        SummaryAction::Wait
    }

    /// Drain pending text into a Summarize action (or Wait if empty).
    fn flush(&mut self) -> SummaryAction {
        if self.pending.is_empty() {
            return SummaryAction::Wait;
        }
        let joined = std::mem::take(&mut self.pending).join(" ");
        SummaryAction::Summarize(joined)
    }

    /// Pending segment count (for tests / introspection).
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fin(text: &str, ms: u64) -> TranscriptSegment {
        TranscriptSegment::final_("s0", text, ms)
    }

    #[test]
    fn fires_after_n_segments() {
        let policy = SummaryPolicy {
            every_n_segments: 3,
            silence_ms: 10_000,
        };
        let mut s = Summarizer::new(policy);
        assert_eq!(s.ingest_final(&fin("one", 0)), SummaryAction::Wait);
        assert_eq!(s.ingest_final(&fin("two", 100)), SummaryAction::Wait);
        // Third final hits the threshold → summarize over joined text.
        assert_eq!(
            s.ingest_final(&fin("three", 200)),
            SummaryAction::Summarize("one two three".to_string())
        );
        // Buffer drained after firing.
        assert_eq!(s.pending_count(), 0);
    }

    #[test]
    fn interim_does_not_count() {
        let mut s = Summarizer::new(SummaryPolicy {
            every_n_segments: 2,
            silence_ms: 10_000,
        });
        // An interim is ignored entirely.
        assert_eq!(
            s.ingest_final(&TranscriptSegment::interim("s0", "live", 0)),
            SummaryAction::Wait
        );
        assert_eq!(s.pending_count(), 0);
    }

    #[test]
    fn fires_on_silence_tick() {
        let mut s = Summarizer::new(SummaryPolicy {
            every_n_segments: 100,
            silence_ms: 5_000,
        });
        s.ingest_final(&fin("hello", 1_000));
        // Not enough silence yet.
        assert_eq!(s.on_tick(3_000), SummaryAction::Wait);
        // 1_000 → 6_000 is 5s of silence → fire.
        assert_eq!(
            s.on_tick(6_000),
            SummaryAction::Summarize("hello".to_string())
        );
    }

    #[test]
    fn tick_with_no_pending_is_noop() {
        let mut s = Summarizer::new(SummaryPolicy::default());
        assert_eq!(s.on_tick(1_000_000), SummaryAction::Wait);
    }

    #[test]
    fn empty_final_ignored() {
        let mut s = Summarizer::new(SummaryPolicy {
            every_n_segments: 1,
            silence_ms: 10_000,
        });
        assert_eq!(s.ingest_final(&fin("   ", 0)), SummaryAction::Wait);
        assert_eq!(s.pending_count(), 0);
    }
}
