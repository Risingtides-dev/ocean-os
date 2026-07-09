//! Status-line segments — the workbench's always-on dashboard, ported from
//! oh-my-pi's composable status bar (OMP Slice 4). The bottom row is built from
//! a small set of independent segments (focus · model · git · rate · session ·
//! advisor · message) rather than one flat string, so each reads at a glance
//! and adding one is a single entry.
//!
//! The *formatting* lives here as pure functions so it's unit-testable without
//! a terminal; `app::draw_status` composes the returned segments with theme
//! colours. A segment whose value is absent (no model bound, not a git repo,
//! no turn run yet) is simply skipped — the bar never shows empty slots.

use super::git;

/// A colour role for a segment, mapped to a concrete `theme::` colour by the
/// renderer. Kept as a small enum (not a ratatui `Color`) so this module stays
/// terminal-agnostic and testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// The focused-pane chip (accent bed).
    Focus,
    /// Primary info (model, session) — foreground.
    Primary,
    /// Muted secondary info — comment grey.
    Muted,
    /// Positive/clean (no dirty files, ahead).
    Ok,
    /// Attention (dirty working tree, behind).
    Warn,
}

/// One rendered segment: its text and colour role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub tone: Tone,
}

impl Segment {
    fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }
}

/// Everything the status bar draws from, snapshotted once per frame.
pub struct StatusData<'a> {
    /// Focused pane label (`chat`, `files`, …).
    pub focus: &'a str,
    /// Model driving turns (the chat pill), if bound.
    pub model: Option<&'a str>,
    /// Cached git status of the active workspace (skipped when not a repo).
    pub git: Option<&'a git::Status>,
    /// Output tokens/second of the last completed turn.
    pub tok_per_s: Option<f64>,
    /// Accumulated output tokens this session.
    pub session_tokens: u64,
    /// Short session id (first 8 chars), if a session is bound.
    pub session: Option<&'a str>,
    /// Advisor state: `Some(model)` when enabled, `Some("off")` when explicitly
    /// disabled, `None` when deferring to the daemon default.
    pub advisor: Option<&'a str>,
    /// The transient status/error message.
    pub message: &'a str,
}

/// Build the ordered segment list. The focus chip and message always render;
/// the middle segments appear only when they have a value.
pub fn segments(d: &StatusData) -> Vec<Segment> {
    let mut out = vec![Segment::new(format!(" {} ", d.focus), Tone::Focus)];

    if let Some(m) = d.model {
        out.push(Segment::new(m.to_string(), Tone::Primary));
    }
    if let Some(g) = d.git {
        if g.is_repo {
            out.push(git_segment(g));
        }
    }
    if let Some(rate) = d.tok_per_s {
        out.push(Segment::new(fmt_rate(rate), Tone::Muted));
    }
    if d.session_tokens > 0 {
        out.push(Segment::new(
            format!("{} tok", fmt_count(d.session_tokens)),
            Tone::Muted,
        ));
    }
    if let Some(s) = d.session {
        out.push(Segment::new(format!("§{s}"), Tone::Muted));
    }
    if let Some(a) = d.advisor {
        out.push(Segment::new(format!("advisor:{a}"), Tone::Muted));
    }
    if !d.message.trim().is_empty() {
        out.push(Segment::new(d.message.to_string(), Tone::Muted));
    }
    out
}

/// Git segment: `branch ±dirty ↑ahead ↓behind`. Clean tree → `Ok` tone (no
/// dirty count); dirty tree → `Warn`. Ahead/behind counts append only when
/// non-zero.
fn git_segment(g: &git::Status) -> Segment {
    let mut text = g.branch.clone();
    let tone = if g.dirty > 0 {
        text.push_str(&format!(" ±{}", g.dirty));
        Tone::Warn
    } else {
        Tone::Ok
    };
    if g.ahead > 0 {
        text.push_str(&format!(" ↑{}", g.ahead));
    }
    if g.behind > 0 {
        text.push_str(&format!(" ↓{}", g.behind));
    }
    Segment::new(text, tone)
}

/// `1.2k/s` / `840/s` — output tokens per second, compact.
fn fmt_rate(rate: f64) -> String {
    format!("{}/s", fmt_count(rate.max(0.0).round() as u64))
}

/// Compact count: `840`, `1.2k`, `12k`, `1.4M`.
fn fmt_count(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        let k = n as f64 / 1_000.0;
        if k < 10.0 {
            format!("{k:.1}k")
        } else {
            format!("{}k", k.round() as u64)
        }
    } else {
        let m = n as f64 / 1_000_000.0;
        format!("{m:.1}M")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> StatusData<'static> {
        StatusData {
            focus: "chat",
            model: None,
            git: None,
            tok_per_s: None,
            session_tokens: 0,
            session: None,
            advisor: None,
            message: "",
        }
    }

    #[test]
    fn empty_state_is_just_the_focus_chip() {
        let segs = segments(&base());
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].tone, Tone::Focus);
        assert_eq!(segs[0].text.trim(), "chat");
    }

    #[test]
    fn absent_values_are_skipped_no_empty_slots() {
        let mut d = base();
        d.model = Some("claude-sonnet-5");
        d.session = Some("2342d9fa");
        let segs = segments(&d);
        let texts: Vec<&str> = segs.iter().map(|s| s.text.as_str()).collect();
        assert!(texts.iter().any(|t| *t == "claude-sonnet-5"));
        assert!(texts.iter().any(|t| t.contains("2342d9fa")));
        // No git / rate / tokens / advisor segments when their values are None.
        assert!(!texts.iter().any(|t| t.contains("±") || t.contains("/s") || t.contains("advisor")));
    }

    #[test]
    fn git_clean_vs_dirty_tone_and_counts() {
        let clean = git::Status {
            is_repo: true,
            branch: "main".into(),
            dirty: 0,
            ahead: 0,
            behind: 0,
        };
        let seg = git_segment(&clean);
        assert_eq!(seg.text, "main");
        assert_eq!(seg.tone, Tone::Ok);

        let dirty = git::Status {
            is_repo: true,
            branch: "feat/x".into(),
            dirty: 3,
            ahead: 2,
            behind: 1,
        };
        let seg = git_segment(&dirty);
        assert_eq!(seg.text, "feat/x ±3 ↑2 ↓1");
        assert_eq!(seg.tone, Tone::Warn);
    }

    #[test]
    fn count_and_rate_formatting() {
        assert_eq!(fmt_count(840), "840");
        assert_eq!(fmt_count(1_200), "1.2k");
        assert_eq!(fmt_count(12_000), "12k");
        assert_eq!(fmt_count(1_400_000), "1.4M");
        assert_eq!(fmt_rate(1234.0), "1.2k/s");
        assert_eq!(fmt_rate(840.4), "840/s");
    }

    #[test]
    fn advisor_and_tokens_segments_present_when_set() {
        let mut d = base();
        d.advisor = Some("claude-haiku-4-5");
        d.session_tokens = 15_400;
        d.tok_per_s = Some(1180.0);
        let texts: Vec<String> = segments(&d).iter().map(|s| s.text.clone()).collect();
        assert!(texts.iter().any(|t| t == "advisor:claude-haiku-4-5"));
        assert!(texts.iter().any(|t| t == "15k tok"));
        assert!(texts.iter().any(|t| t == "1.2k/s"));
    }
}
