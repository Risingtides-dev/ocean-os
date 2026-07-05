//! Slash-command registry + a tiny inline fuzzy matcher.
//!
//! The composer opens a floating command palette whenever its text starts with
//! `/` (see `components::chat`). This module owns the *data* — the built-in set
//! of commands — and the *ranking* — a small subsequence fuzzy matcher — so both
//! stay pure and unit-testable without a terminal.
//!
//! Adding a command is one line in [`COMMANDS`]. Execution is wired in
//! `chat::run_slash`, keyed on `name`.

/// One entry in the palette. `name` carries the leading `/` (it's what renders
/// and what execution matches on); `desc` is the one-line hint. `soon` marks a
/// roadmap command whose backend isn't built on this branch yet — it renders
/// greyed with a "soon" badge and, when run, surfaces an honest "not wired"
/// hint instead of pretending. The palette is the discoverability surface for
/// Ocean's harness capabilities, so the roadmap shows even before it's live.
pub struct SlashCommand {
    pub name: &'static str,
    pub desc: &'static str,
    pub soon: bool,
}

/// The built-in command set. One line per command — the palette, the fuzzy
/// filter, and `/help` all read from here, so this is the single source of
/// truth. Keep names short and lower-case; execution matches on `name`.
///
/// LIVE commands work when pressed. `soon: true` commands advertise where the
/// harness is going (the W3–W7 slices) and say so honestly when run.
pub const COMMANDS: &[SlashCommand] = &[
    // ── live: wired to real behavior ──────────────────────────────────────
    SlashCommand { name: "/new", desc: "start a fresh session", soon: false },
    SlashCommand { name: "/model", desc: "set the model for this session (/model <id>)", soon: false },
    SlashCommand { name: "/copy", desc: "copy the last reply to the clipboard", soon: false },
    SlashCommand { name: "/resume", desc: "resume a past session", soon: false },
    SlashCommand { name: "/sessions", desc: "focus the session rail", soon: false },
    SlashCommand { name: "/files", desc: "focus the file tree", soon: false },
    SlashCommand { name: "/graph", desc: "open the graph view", soon: false },
    SlashCommand { name: "/terminal", desc: "focus the terminal", soon: false },
    SlashCommand { name: "/clear", desc: "clear the chat transcript", soon: false },
    SlashCommand { name: "/help", desc: "list all commands", soon: false },
    SlashCommand { name: "/quit", desc: "exit ocean", soon: false },
    // ── soon: roadmap surface (W3–W7), honest when run ────────────────────
    SlashCommand { name: "/compact", desc: "prune + shake the context (W3)", soon: true },
    SlashCommand { name: "/context", desc: "context-economy panel (W3)", soon: true },
    SlashCommand { name: "/diff", desc: "review pending edits (W3)", soon: true },
    SlashCommand { name: "/lsp", desc: "diagnostics / rename (W5)", soon: true },
    SlashCommand { name: "/rules", desc: "manage stream rules (W6)", soon: true },
    SlashCommand { name: "/memory", desc: "recall / retain memory — OKF (W7)", soon: true },
    SlashCommand { name: "/goal", desc: "set the session goal (W7)", soon: true },
    SlashCommand { name: "/handoff", desc: "write handoff.md (W7)", soon: true },
];

/// Is `name` (with leading `/`) a known command? Used by the composer to decide
/// whether a typed `/foo bar` line is a command invocation or a plain message.
pub fn is_command(name: &str) -> bool {
    COMMANDS.iter().any(|c| c.name == name)
}

/// Filter + rank the registry against `query` (the composer text *after* the
/// leading `/`). Returns matches best-first, each paired with its score. An
/// empty query matches everything (score 0) so the bare `/` shows the full
/// palette. Non-subsequence commands are dropped.
///
/// Ties break toward the shorter name, then alphabetically, so the ordering is
/// deterministic (important for the selection cursor and for tests).
pub fn filter(query: &str) -> Vec<(&'static SlashCommand, i32)> {
    let mut out: Vec<(&'static SlashCommand, i32)> = COMMANDS
        .iter()
        .filter_map(|c| fuzzy_score(query, c).map(|s| (c, s)))
        .collect();
    out.sort_by(|a, b| {
        b.1.cmp(&a.1)
            // live (soon=false) sorts ahead of roadmap (soon=true) on score ties,
            // so the bare `/` menu groups working commands above the roadmap.
            .then_with(|| a.0.soon.cmp(&b.0.soon))
            .then_with(|| a.0.name.len().cmp(&b.0.name.len()))
            .then_with(|| a.0.name.cmp(b.0.name))
    });
    out
}

/// Score `query` against one command's name (leading `/` ignored, case-folded).
/// Subsequence match required — returns `None` if any needle char is missing in
/// order. Score rewards start-of-name hits, contiguous runs, and a whole-query
/// prefix, so `/model` beats a scattered match for `mod`.
fn fuzzy_score(query: &str, cmd: &SlashCommand) -> Option<i32> {
    subseq_score(query, cmd.name.trim_start_matches('/'))
}

/// Subsequence fuzzy scorer over an arbitrary haystack (case-folded). Shared by
/// the `/` palette and the ⌃R prompt-history search so both rank identically.
/// Returns `None` when `query` is not an in-order subsequence of `hay`; an empty
/// query scores 0 (matches everything). Rewards start-of-string hits, contiguous
/// runs, and a whole-query prefix.
pub(crate) fn subseq_score(query: &str, hay: &str) -> Option<i32> {
    let name = hay.to_lowercase();
    let q = query.to_lowercase();
    if q.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = name.chars().collect();
    let mut score = 0i32;
    let mut hi = 0usize;
    let mut last_match: Option<usize> = None;
    for nc in q.chars() {
        let mut matched_at = None;
        while hi < hay.len() {
            if hay[hi] == nc {
                matched_at = Some(hi);
                break;
            }
            hi += 1;
        }
        let mi = matched_at?;
        score += 2; // base per matched char
        if mi == 0 {
            score += 8; // start-of-name hit
        }
        if let Some(lm) = last_match {
            if mi == lm + 1 {
                score += 6; // contiguous with previous match
            }
        }
        last_match = Some(mi);
        hi = mi + 1;
    }
    if name.starts_with(&q) {
        score += 12; // whole-query prefix bonus
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_non_empty() {
        assert!(!COMMANDS.is_empty());
        // Names must carry the leading slash — execution matches on it.
        assert!(COMMANDS.iter().all(|c| c.name.starts_with('/')));
    }

    #[test]
    fn empty_query_matches_all() {
        assert_eq!(filter("").len(), COMMANDS.len());
    }

    #[test]
    fn mod_ranks_model_first() {
        let ranked = filter("mod");
        assert_eq!(ranked[0].0.name, "/model", "\"mod\" should rank /model first");
    }

    #[test]
    fn prefix_beats_scattered() {
        // "s" is a prefix of /sessions but appears mid-word in /files, /resume.
        let ranked = filter("s");
        assert_eq!(ranked[0].0.name, "/sessions");
    }

    #[test]
    fn prefix_completion_picks_help() {
        // Typing "hel" should surface /help as the top completion.
        let ranked = filter("hel");
        assert_eq!(ranked[0].0.name, "/help");
    }

    #[test]
    fn contiguous_outranks_gapped() {
        // "term" is contiguous in /terminal; no other command out-scores it.
        let ranked = filter("term");
        assert_eq!(ranked[0].0.name, "/terminal");
    }

    #[test]
    fn non_subsequence_is_dropped() {
        // "zzz" is a subsequence of nothing in the registry.
        assert!(filter("zzz").is_empty());
    }

    #[test]
    fn live_commands_group_ahead_of_roadmap() {
        // On the bare `/` menu, every live command sorts above every soon one.
        let ranked = filter("");
        let first_soon = ranked.iter().position(|(c, _)| c.soon);
        let last_live = ranked.iter().rposition(|(c, _)| !c.soon);
        if let (Some(fs), Some(ll)) = (first_soon, last_live) {
            assert!(ll < fs, "a soon command sorted ahead of a live one");
        }
    }

    #[test]
    fn is_command_recognizes_registry_only() {
        assert!(is_command("/model"));
        assert!(is_command("/compact"));
        assert!(!is_command("/home")); // a path, not a command
        assert!(!is_command("/nope"));
    }

    #[test]
    fn ranking_is_deterministic() {
        // Same query, same order, every call.
        assert_eq!(
            filter("e").iter().map(|(c, _)| c.name).collect::<Vec<_>>(),
            filter("e").iter().map(|(c, _)| c.name).collect::<Vec<_>>(),
        );
    }
}
