//! Typed status/health state + the bottom row's pure segment selection.
//!
//! Health is tracked per SOURCE (daemon probe, SSE transport) so a recovery
//! clears only its own source — the effective indication stays degraded while
//! ANY source remains degraded. Healthy/recovered success text is never
//! rendered; the segment simply disappears.
//!
//! The bottom row selects, in priority order: model, effective degraded
//! health, unresolved error/notice, live activity, exceptional Git. Selection
//! is width-aware — lowest-priority context clips first, whole segments at a
//! time, and the model is never dropped. Formatting lives here as pure
//! functions so it's unit-testable without a terminal; `app::draw_status`
//! composes the returned segments with theme colours.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::action::HealthSource;
use super::git;

/// Independently-clearable degraded state per health source. `Some(condition)`
/// = degraded with that terse condition; `None` = healthy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Health {
    daemon: Option<String>,
    sse: Option<String>,
}

impl Health {
    /// Mark `source` degraded with `condition` (replaces the source's prior
    /// condition; never touches the other source).
    pub fn degrade(&mut self, source: HealthSource, condition: String) {
        match source {
            HealthSource::Daemon => self.daemon = Some(condition),
            HealthSource::Sse => self.sse = Some(condition),
        }
    }

    /// Mark `source` healthy. Clears ONLY that source — a daemon recovery must
    /// not mask a still-degraded stream, and vice versa.
    pub fn recover(&mut self, source: HealthSource) {
        match source {
            HealthSource::Daemon => self.daemon = None,
            HealthSource::Sse => self.sse = None,
        }
    }

    /// The effective degraded condition, if any. The daemon probe outranks the
    /// SSE transport: a dead daemon explains a dead stream, so it reads first.
    pub fn effective(&self) -> Option<&str> {
        self.daemon.as_deref().or(self.sse.as_deref())
    }
}

/// A colour role for a segment, mapped to a concrete `theme::` colour by the
/// renderer. Kept as a small enum (not a ratatui `Color`) so this module stays
/// terminal-agnostic and testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Primary info (the bound model) — foreground.
    Primary,
    /// Muted contextual info (activity) — comment grey.
    Muted,
    /// Attention (degraded health, unresolved error, exceptional Git).
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
            text: sanitize(&text.into()),
            tone,
        }
    }
}

/// Layout-safe text: newlines/tabs become spaces, remaining control chars are
/// stripped. Daemon-fed strings (model ids, error bodies) are untrusted for
/// layout and must never wrap or corrupt the single status row.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect()
}

/// Everything the bottom row draws from, snapshotted once per frame.
pub struct StatusData<'a> {
    /// Model driving turns, if bound. Highest priority — never clipped.
    pub model: Option<&'a str>,
    /// Effective degraded health condition ([`Health::effective`]).
    pub health: Option<&'a str>,
    /// Unresolved error/notice message (empty/whitespace = nothing to show).
    pub error: Option<&'a str>,
    /// Live turn/tool activity, derived from chat state (`working` or the
    /// current tool name). `None` when idle — never a stale copy.
    pub activity: Option<&'a str>,
    /// Cached git status; the branch renders whenever the workspace is a
    /// repo, the counts (and Warn tone) only when nonzero.
    pub git: Option<&'a git::Status>,
    /// Last finished turn's tokens/sec, as the daemon reported it (provider
    /// usage when available, daemon estimate otherwise). `None` = no reading.
    pub tok_per_s: Option<f64>,
}

/// Width of the two-space separator between rendered segments (plain ASCII —
/// no decorative interpuncts on UI surfaces).
const SEP_W: usize = 2;

/// Build the ordered segment list, clipping to `max_width` display columns.
/// LAYOUT order: model · branch · health · error · activity · tok/s.
/// SURVIVAL is separate: on overflow whole segments drop by rank — tok/s
/// first, then activity, then branch, then health/error; the model (identity,
/// rank 0) never drops. Whatever survives alone is width-clamped so a single
/// long name can't overflow the row.
pub fn segments(d: &StatusData, max_width: usize) -> Vec<Segment> {
    // (segment, drop_rank) — higher rank drops first; ties drop rightmost.
    let mut ranked: Vec<(Segment, u8)> = Vec::new();
    if let Some(m) = d.model {
        ranked.push((Segment::new(m.to_string(), Tone::Primary), 0));
    }
    if let Some(g) = d.git {
        if let Some(seg) = git_segment(g) {
            ranked.push((seg, 2));
        }
    }
    if let Some(h) = d.health {
        ranked.push((Segment::new(h.to_string(), Tone::Warn), 1));
    }
    if let Some(e) = d.error.filter(|e| !e.trim().is_empty()) {
        ranked.push((Segment::new(e.to_string(), Tone::Warn), 1));
    }
    if let Some(a) = d.activity {
        ranked.push((Segment::new(a.to_string(), Tone::Muted), 3));
    }
    if let Some(t) = d.tok_per_s {
        ranked.push((Segment::new(format!("{t:.0} tok/s"), Tone::Muted), 4));
    }
    while ranked.len() > 1 && ranked_row_width(&ranked) > max_width {
        let idx = ranked
            .iter()
            .enumerate()
            .max_by_key(|(i, (_, rank))| (*rank, *i))
            .map(|(i, _)| i)
            .expect("non-empty");
        ranked.remove(idx);
    }
    let mut out: Vec<Segment> = ranked.into_iter().map(|(s, _)| s).collect();
    if let [only] = out.as_mut_slice() {
        let budget = max_width.saturating_sub(1); // leading pad space
        if only.text.width() > budget {
            only.text = truncate_cells(&only.text, budget);
        }
    }
    out
}

/// [`row_width`] over the ranked working list.
fn ranked_row_width(segs: &[(Segment, u8)]) -> usize {
    let text: usize = segs.iter().map(|(s, _)| s.text.width()).sum();
    text + segs.len().saturating_sub(1) * SEP_W + 1
}

/// Hard-clip `s` to at most `max` display cells (no ellipsis — plain clip).
fn truncate_cells(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

/// Total display cells of `segs` rendered with two-space separators and the
/// leading pad space.
fn row_width(segs: &[Segment]) -> usize {
    let text: usize = segs.iter().map(|s| s.text.width()).sum();
    text + segs.len().saturating_sub(1) * SEP_W + 1
}

/// Git: `branch [~dirty +ahead -behind]` (plain ASCII). The branch is
/// identity and renders whenever the workspace is a repo; the counts stay
/// exceptional — appended (and the tone warns) only when nonzero.
fn git_segment(g: &git::Status) -> Option<Segment> {
    if !g.is_repo || g.branch.is_empty() {
        return None;
    }
    let mut text = g.branch.clone();
    if g.dirty > 0 {
        text.push_str(&format!(" ~{}", g.dirty));
    }
    if g.ahead > 0 {
        text.push_str(&format!(" +{}", g.ahead));
    }
    if g.behind > 0 {
        text.push_str(&format!(" -{}", g.behind));
    }
    let exceptional = g.dirty > 0 || g.ahead > 0 || g.behind > 0;
    Some(Segment::new(
        text,
        if exceptional { Tone::Warn } else { Tone::Muted },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> StatusData<'static> {
        StatusData {
            model: None,
            health: None,
            error: None,
            activity: None,
            tok_per_s: None,
            git: None,
        }
    }

    #[test]
    fn idle_healthy_state_shows_model_only() {
        let mut d = base();
        d.model = Some("claude-sonnet-5");
        let segs = segments(&d, 120);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "claude-sonnet-5");
        assert_eq!(segs[0].tone, Tone::Primary);
    }

    #[test]
    fn empty_state_renders_nothing() {
        assert!(segments(&base(), 120).is_empty());
    }

    #[test]
    fn health_daemon_outranks_sse_and_recovery_clears_own_source_only() {
        let mut h = Health::default();
        h.degrade(HealthSource::Sse, "stream reconnecting".into());
        h.degrade(HealthSource::Daemon, "daemon offline".into());
        assert_eq!(h.effective(), Some("daemon offline"));
        // Daemon recovers — the still-degraded stream must NOT be masked.
        h.recover(HealthSource::Daemon);
        assert_eq!(h.effective(), Some("stream reconnecting"));
        // Stream recovers — all healthy, nothing rendered.
        h.recover(HealthSource::Sse);
        assert_eq!(h.effective(), None);
    }

    #[test]
    fn recovery_of_one_source_never_touches_the_other() {
        let mut h = Health::default();
        h.degrade(HealthSource::Daemon, "daemon offline".into());
        h.recover(HealthSource::Sse); // unrelated recovery
        assert_eq!(h.effective(), Some("daemon offline"));
    }

    #[test]
    fn git_branch_always_renders_counts_stay_exceptional() {
        let clean = git::Status {
            is_repo: true,
            branch: "main".into(),
            dirty: 0,
            ahead: 0,
            behind: 0,
        };
        let seg = git_segment(&clean).expect("branch is identity — always renders in a repo");
        assert_eq!(seg.text, "main", "clean tree shows the bare branch");
        assert_eq!(
            seg.tone,
            Tone::Muted,
            "clean branch is muted, not a warning"
        );

        let dirty = git::Status {
            is_repo: true,
            branch: "feat/x".into(),
            dirty: 3,
            ahead: 2,
            behind: 1,
        };
        let seg = git_segment(&dirty).expect("exceptional git renders");
        assert_eq!(seg.text, "feat/x ~3 +2 -1");
        assert_eq!(seg.tone, Tone::Warn);

        let not_repo = git::Status {
            is_repo: false,
            branch: String::new(),
            dirty: 0,
            ahead: 0,
            behind: 0,
        };
        assert!(git_segment(&not_repo).is_none(), "no repo, no segment");
    }

    #[test]
    fn width_matrix_drops_by_rank_health_outlives_extras_model_survives() {
        let git = git::Status {
            is_repo: true,
            branch: "feat/x".into(),
            dirty: 3,
            ahead: 0,
            behind: 0,
        };
        let mut d = base();
        d.model = Some("claude-sonnet-5");
        d.health = Some("daemon offline");
        d.activity = Some("working");
        d.tok_per_s = Some(42.0);
        d.git = Some(&git);
        let texts =
            |w: usize| -> Vec<String> { segments(&d, w).iter().map(|s| s.text.clone()).collect() };
        // Wide: everything fits, LAYOUT order (branch beside the model).
        assert_eq!(
            texts(200),
            vec![
                "claude-sonnet-5",
                "feat/x ~3",
                "daemon offline",
                "working",
                "42 tok/s"
            ]
        );
        // Shrinking drops by RANK, not position: tok/s first…
        assert_eq!(
            texts(55),
            vec!["claude-sonnet-5", "feat/x ~3", "daemon offline", "working"]
        );
        // …then activity…
        assert_eq!(
            texts(45),
            vec!["claude-sonnet-5", "feat/x ~3", "daemon offline"]
        );
        // …then the branch — HEALTH outlives every optional segment even
        // though it renders to the branch's right.
        assert_eq!(texts(40), vec!["claude-sonnet-5", "daemon offline"]);
        // …then health, leaving identity…
        assert_eq!(texts(20), vec!["claude-sonnet-5"]);
        // …which survives alone, hard-clipped to the row.
        assert_eq!(texts(10), vec!["claude-so"]);
    }

    #[test]
    fn blank_error_is_not_a_segment() {
        let mut d = base();
        d.model = Some("m");
        d.error = Some("   ");
        assert_eq!(segments(&d, 120).len(), 1);
    }
}
