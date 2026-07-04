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
/// and what execution matches on); `desc` is the one-line hint.
pub struct SlashCommand {
    pub name: &'static str,
    pub desc: &'static str,
}

/// The built-in command set. One line per command — the palette, the fuzzy
/// filter, and `/help` all read from here, so this is the single source of
/// truth. Keep names short and lower-case; execution matches on `name`.
pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand { name: "/model", desc: "switch the active model" },
    SlashCommand { name: "/clear", desc: "clear the chat transcript" },
    SlashCommand { name: "/sessions", desc: "focus the session rail" },
    SlashCommand { name: "/files", desc: "focus the file tree" },
    SlashCommand { name: "/graph", desc: "open the graph view" },
    SlashCommand { name: "/terminal", desc: "focus the terminal" },
    SlashCommand { name: "/resume", desc: "resume a past session" },
    SlashCommand { name: "/help", desc: "list all commands" },
    SlashCommand { name: "/quit", desc: "exit ocean" },
];

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
    let name = cmd.name.trim_start_matches('/').to_lowercase();
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
    fn ranking_is_deterministic() {
        // Same query, same order, every call.
        assert_eq!(
            filter("e").iter().map(|(c, _)| c.name).collect::<Vec<_>>(),
            filter("e").iter().map(|(c, _)| c.name).collect::<Vec<_>>(),
        );
    }
}
