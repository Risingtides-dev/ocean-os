//! Wake-word active lane — the mute-gate on Ocean's voice during a call.
//!
//! The passive lane is always on and never speaks. The **active lane** lets a
//! participant summon Ocean to answer out loud: "hey Ocean, what did we decide
//! about the release date?". The wake word exists precisely to keep Ocean
//! *contained* — it speaks only when called, never blurts into the call.
//!
//! The spec's containment rules, all enforced here:
//! - speaks only inside an open wake window (one ephemeral turn per trigger),
//! - a cooldown after speaking so Ocean can't re-trigger on its own TTS echo,
//! - a global `voice_muted` flag so a sensitive (label) call can be
//!   passive-only.
//!
//! This is pure routing logic over already-transcribed text + a clock value;
//! the actual agent turn and TTS playback live in the adapter. Fully testable
//! without audio or a network.

/// Outcome of a wake-phrase match (shared shape with the surface voice agent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeMatch {
    /// No wake phrase at the start of the utterance.
    None,
    /// Wake phrase with no trailing command — arm for the next utterance.
    Armed,
    /// Wake phrase followed by a command to answer now.
    Command(String),
}

const WAKE_PHRASES: &[&str] = &["hey ocean", "ok ocean", "okay ocean", "hi ocean", "ocean"];

/// Normalize STT text: lowercase, collapse non-alphanumerics to single spaces.
fn normalize(text: &str) -> String {
    let lowered = text.to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut prev_space = true;
    for ch in lowered.chars() {
        if ch.is_alphanumeric() {
            out.push(ch);
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim_end().to_string()
}

/// Match a transcript for a leading wake phrase (word-boundary anchored).
pub fn match_wake(text: &str) -> WakeMatch {
    let norm = normalize(text);
    if norm.is_empty() {
        return WakeMatch::None;
    }
    for phrase in WAKE_PHRASES {
        if norm == *phrase {
            return WakeMatch::Armed;
        }
        let with_space = format!("{phrase} ");
        if let Some(rest) = norm.strip_prefix(&with_space) {
            let rest = rest.trim();
            return if rest.is_empty() {
                WakeMatch::Armed
            } else {
                WakeMatch::Command(rest.to_string())
            };
        }
    }
    WakeMatch::None
}

/// What the active lane should do with an utterance.
#[derive(Debug, Clone, PartialEq)]
pub enum WakeAction {
    /// Stay silent — not summoned, muted, or cooling down.
    Silent,
    /// Run one ephemeral agent turn over this command and speak the reply.
    Answer(String),
}

/// The wake-gated voice state machine for one call.
#[derive(Debug)]
pub struct WakeGate {
    /// Global mute — passive-only when true (sensitive calls).
    voice_muted: bool,
    /// Cooldown after a reply, in ms, to avoid echo re-triggering.
    cooldown_ms: u64,
    /// Heard a bare wake word; next utterance is the command.
    armed: bool,
    /// Wall-clock ms when the last reply finished; 0 = never.
    last_reply_ms: u64,
}

impl WakeGate {
    pub fn new(voice_muted: bool, cooldown_ms: u64) -> Self {
        Self {
            voice_muted,
            cooldown_ms,
            armed: false,
            last_reply_ms: 0,
        }
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.voice_muted = muted;
        if muted {
            self.armed = false;
        }
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// Process an utterance at wall-clock `now_ms`. Returns whether Ocean should
    /// answer (and over what command).
    pub fn on_utterance(&mut self, text: &str, now_ms: u64) -> WakeAction {
        if self.voice_muted {
            return WakeAction::Silent;
        }
        // Within cooldown after the last reply: ignore (likely our own echo).
        if self.last_reply_ms != 0 && now_ms.saturating_sub(self.last_reply_ms) < self.cooldown_ms {
            return WakeAction::Silent;
        }
        let text = text.trim();
        if text.is_empty() {
            return WakeAction::Silent;
        }

        if self.armed {
            self.armed = false;
            return WakeAction::Answer(text.to_string());
        }
        match match_wake(text) {
            WakeMatch::None => WakeAction::Silent,
            WakeMatch::Armed => {
                self.armed = true;
                WakeAction::Silent
            }
            WakeMatch::Command(cmd) => WakeAction::Answer(cmd),
        }
    }

    /// Call after Ocean finishes speaking, to start the cooldown window.
    pub fn mark_replied(&mut self, now_ms: u64) {
        self.last_reply_ms = now_ms;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_with_command_answers() {
        let mut g = WakeGate::new(false, 2_000);
        assert_eq!(
            g.on_utterance("hey Ocean what did we decide", 1_000),
            WakeAction::Answer("what did we decide".to_string())
        );
    }

    #[test]
    fn bare_wake_arms_then_next_is_command() {
        let mut g = WakeGate::new(false, 2_000);
        assert_eq!(g.on_utterance("hey ocean", 1_000), WakeAction::Silent);
        assert!(g.is_armed());
        assert_eq!(
            g.on_utterance("summarize the call", 1_500),
            WakeAction::Answer("summarize the call".to_string())
        );
    }

    #[test]
    fn unsummoned_chatter_stays_silent() {
        let mut g = WakeGate::new(false, 2_000);
        assert_eq!(
            g.on_utterance("so anyway the release is friday", 1_000),
            WakeAction::Silent
        );
    }

    #[test]
    fn muted_never_answers() {
        let mut g = WakeGate::new(true, 2_000);
        assert_eq!(
            g.on_utterance("hey ocean what's up", 1_000),
            WakeAction::Silent
        );
    }

    #[test]
    fn cooldown_suppresses_echo_after_reply() {
        let mut g = WakeGate::new(false, 2_000);
        // Ocean answers, then marks the reply time.
        assert!(matches!(
            g.on_utterance("hey ocean hi", 1_000),
            WakeAction::Answer(_)
        ));
        g.mark_replied(1_200);
        // An utterance during cooldown (e.g. Ocean's own TTS) is ignored.
        assert_eq!(
            g.on_utterance("hey ocean hi", 2_000),
            WakeAction::Silent
        );
        // After cooldown elapses, it works again.
        assert!(matches!(
            g.on_utterance("hey ocean status", 3_500),
            WakeAction::Answer(_)
        ));
    }

    #[test]
    fn muting_disarms() {
        let mut g = WakeGate::new(false, 2_000);
        g.on_utterance("hey ocean", 1_000);
        assert!(g.is_armed());
        g.set_muted(true);
        assert!(!g.is_armed());
    }

    #[test]
    fn non_leading_mention_does_not_trigger() {
        let mut g = WakeGate::new(false, 2_000);
        assert_eq!(
            g.on_utterance("tell ocean we said hi", 1_000),
            WakeAction::Silent
        );
    }
}
