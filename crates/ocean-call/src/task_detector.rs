//! Task / action-item detection over call transcript segments.
//!
//! The passive lane watches the transcript for commitments and action items
//! ("I'll send the master tonight", "we need to verify the toll-free number",
//! "can you follow up with Atlantic"). The spec is explicit: **detect-and-notify
//! only** — a detected task is surfaced as a `CallTaskDetected` event for John
//! to approve; acting on it is always a separate, human-approved turn. Nothing
//! here executes anything.
//!
//! This module is the pure heuristic pre-filter: cheap phrase matching that
//! decides whether a segment *looks like* a task and extracts a candidate
//! title + confidence. An LLM-refined pass (in the adapter) can run on top, but
//! the heuristic alone keeps the lane responsive and is fully unit-testable.

use crate::stt::TranscriptSegment;

/// A detected candidate task. Maps onto the `CallTaskDetected` OceanEvent.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedTask {
    pub title: String,
    /// 0.0–1.0 heuristic confidence that this is a real action item.
    pub confidence: f32,
    /// The transcript text the detection came from.
    pub source_quote: String,
}

/// Phrases that signal a commitment or assigned action. Matched case-insensitively
/// against the segment text. Each carries a base confidence.
const TASK_CUES: &[(&str, f32)] = &[
    ("i'll ", 0.7),
    ("i will ", 0.7),
    ("we'll ", 0.6),
    ("we will ", 0.6),
    ("we need to ", 0.65),
    ("need to ", 0.55),
    ("can you ", 0.6),
    ("could you ", 0.6),
    ("let's ", 0.5),
    ("make sure ", 0.6),
    ("don't forget ", 0.7),
    ("follow up ", 0.7),
    ("action item", 0.9),
    ("to-do", 0.8),
    ("todo", 0.8),
    ("by friday", 0.55),
    ("by monday", 0.55),
    ("by tomorrow", 0.55),
    ("send ", 0.45),
    ("schedule ", 0.5),
    ("verify ", 0.5),
];

/// Minimum confidence to surface a task at all.
const MIN_CONFIDENCE: f32 = 0.5;

/// Scan a final transcript segment for an action item.
///
/// Returns at most one [`DetectedTask`] per segment (the highest-confidence
/// cue match). Interim segments and empty text return None. Detect-only — the
/// caller decides what to do with the result; this never acts.
pub fn detect(seg: &TranscriptSegment) -> Option<DetectedTask> {
    if !seg.is_final {
        return None;
    }
    let text = seg.text.trim();
    if text.is_empty() {
        return None;
    }
    let lower = text.to_lowercase();

    // Find the strongest matching cue.
    let mut best: Option<f32> = None;
    for (cue, conf) in TASK_CUES {
        if lower.contains(cue) {
            best = Some(best.map_or(*conf, |b| b.max(*conf)));
        }
    }
    let confidence = best?;
    if confidence < MIN_CONFIDENCE {
        return None;
    }

    Some(DetectedTask {
        title: summarize_title(text),
        confidence,
        source_quote: text.to_string(),
    })
}

/// Derive a short title from the segment text: first sentence-ish span, capped.
fn summarize_title(text: &str) -> String {
    // Cut at the first sentence terminator if present.
    let end = text
        .find(['.', '?', '!'])
        .map(|i| i + 1)
        .unwrap_or(text.len());
    let head = text[..end].trim();
    const MAX: usize = 80;
    if head.chars().count() > MAX {
        let truncated: String = head.chars().take(MAX - 1).collect();
        format!("{truncated}…")
    } else {
        head.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fin(text: &str) -> TranscriptSegment {
        TranscriptSegment::final_("s0", text, 0)
    }

    #[test]
    fn detects_commitment() {
        let task = detect(&fin("I'll send the master to Atlantic tonight")).unwrap();
        assert!(task.confidence >= 0.7);
        assert!(task.title.to_lowercase().contains("send the master"));
        assert!(task.source_quote.contains("Atlantic"));
    }

    #[test]
    fn detects_assigned_action() {
        let task = detect(&fin("Can you follow up with the label by Friday?")).unwrap();
        // "can you" + "follow up " + "by friday" all match; takes the max.
        assert!(task.confidence >= 0.7);
    }

    #[test]
    fn detects_explicit_action_item() {
        let task = detect(&fin("Action item: verify the toll-free number")).unwrap();
        assert!(task.confidence >= 0.9);
    }

    #[test]
    fn ignores_chitchat() {
        assert!(detect(&fin("yeah the weather's been great lately")).is_none());
        assert!(detect(&fin("haha that's hilarious")).is_none());
    }

    #[test]
    fn ignores_interim() {
        assert!(detect(&TranscriptSegment::interim("s0", "I'll send it", 0)).is_none());
    }

    #[test]
    fn ignores_empty() {
        assert!(detect(&fin("   ")).is_none());
    }

    #[test]
    fn title_truncates_long_text() {
        let long = "I'll ".to_string() + &"word ".repeat(40);
        let task = detect(&fin(&long)).unwrap();
        assert!(task.title.chars().count() <= 80);
    }

    #[test]
    fn title_stops_at_sentence_end() {
        let task = detect(&fin("I'll call them back. Then we grab lunch.")).unwrap();
        assert_eq!(task.title, "I'll call them back.");
    }
}
