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
    /// Breadcrumb group — sections the bare `/` menu and prefixes filtered rows
    /// (`session › /new`). Keep to one short lowercase word.
    pub group: &'static str,
    pub soon: bool,
}

/// The built-in command set. One line per command — the palette, the fuzzy
/// filter, and `/help` all read from here, so this is the single source of
/// truth. Keep names short and lower-case; execution matches on `name`.
/// Commands are ordered by group: the bare `/` menu renders these groups as
/// sections (in first-appearance order), so keep each group's lines together.
///
/// LIVE commands work when pressed. `soon: true` commands advertise where the
/// harness is going (the W3–W7 slices) and say so honestly when run.
pub const COMMANDS: &[SlashCommand] = &[
    // ── session ────────────────────────────────────────────────────────────
    SlashCommand {
        name: "/new",
        desc: "start a fresh session",
        group: "session",
        soon: false,
    },
    SlashCommand {
        name: "/resume",
        desc: "resume a past session",
        group: "session",
        soon: false,
    },
    SlashCommand {
        name: "/sessions",
        desc: "focus the session rail",
        group: "session",
        soon: false,
    },
    SlashCommand {
        name: "/models",
        desc: "pick a model + thinking level (live registry)",
        group: "session",
        soon: false,
    },
    SlashCommand {
        name: "/model",
        desc: "set the model directly (/model <id>; bare opens the picker)",
        group: "session",
        soon: false,
    },
    SlashCommand {
        name: "/thinking",
        desc: "set thinking directly (/thinking default|off|minimal|low|medium|high|xhigh)",
        group: "session",
        soon: false,
    },
    SlashCommand {
        name: "/advisor",
        desc: "pick a post-turn advisor model (or off) — a second opinion each turn",
        group: "session",
        soon: false,
    },
    SlashCommand {
        name: "/login",
        desc: "provider logins (popup) or /login [claude|codex] browser flow",
        group: "session",
        soon: false,
    },
    // ── workspace ──────────────────────────────────────────────────────────
    SlashCommand {
        name: "/files",
        desc: "focus the file tree",
        group: "workspace",
        soon: false,
    },
    SlashCommand {
        name: "/graph",
        desc: "open the graph view",
        group: "workspace",
        soon: false,
    },
    SlashCommand {
        name: "/terminal",
        desc: "focus the terminal",
        group: "workspace",
        soon: false,
    },
    SlashCommand {
        name: "/settings",
        desc: "open the settings panel",
        group: "workspace",
        soon: false,
    },
    SlashCommand {
        name: "/permissions",
        desc: "choose when Ocean pauses for approval",
        group: "workspace",
        soon: false,
    },
    SlashCommand {
        name: "/image",
        desc: "view an image inline (/image [path]; bare = newest in chat)",
        group: "workspace",
        soon: false,
    },
    SlashCommand {
        name: "/providers",
        desc: "provider logins & API keys",
        group: "workspace",
        soon: false,
    },
    // ── chat ───────────────────────────────────────────────────────────────
    SlashCommand {
        name: "/copy",
        desc: "copy the last reply to the clipboard",
        group: "chat",
        soon: false,
    },
    SlashCommand {
        name: "/clear",
        desc: "clear the chat transcript",
        group: "chat",
        soon: false,
    },
    SlashCommand {
        name: "/pinned",
        desc: "show or hide the pinned component (/pinned show|hide)",
        group: "chat",
        soon: false,
    },
    SlashCommand {
        name: "/help",
        desc: "list all commands",
        group: "chat",
        soon: false,
    },
    SlashCommand {
        name: "/quit",
        desc: "exit ocean",
        group: "chat",
        soon: false,
    },
    // ── context (W3 roadmap) ───────────────────────────────────────────────
    SlashCommand {
        name: "/compact",
        desc: "summarize older context and keep the recent window",
        group: "context",
        soon: false,
    },
    SlashCommand {
        name: "/context",
        desc: "context-economy panel (W3)",
        group: "context",
        soon: true,
    },
    SlashCommand {
        name: "/diff",
        desc: "review pending edits (W3)",
        group: "context",
        soon: true,
    },
    // ── intel (W5–W6 roadmap) ──────────────────────────────────────────────
    SlashCommand {
        name: "/lsp",
        desc: "language servers for this project (ready / install state)",
        group: "intel",
        soon: false,
    },
    SlashCommand {
        name: "/rules",
        desc: "manage stream rules (W6)",
        group: "intel",
        soon: true,
    },
    // ── agent (W7 roadmap) ─────────────────────────────────────────────────
    SlashCommand {
        name: "/memory",
        desc: "browse + search long-term memories (what the agent retained)",
        group: "agent",
        soon: false,
    },
    SlashCommand {
        name: "/goal",
        desc: "set the session goal (W7)",
        group: "agent",
        soon: true,
    },
    SlashCommand {
        name: "/handoff",
        desc: "write handoff.md (W7)",
        group: "agent",
        soon: true,
    },
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
    // Bare `/`: keep registry order so the menu renders as contiguous group
    // sections (session / workspace / chat / roadmap groups).
    if query.is_empty() {
        return COMMANDS.iter().map(|c| (c, 0)).collect();
    }
    let mut out: Vec<(&'static SlashCommand, i32)> = COMMANDS
        .iter()
        .filter_map(|c| fuzzy_score(query, c).map(|s| (c, s)))
        .collect();
    out.sort_by(|a, b| {
        b.1.cmp(&a.1)
            // live (soon=false) sorts ahead of roadmap (soon=true) on score ties,
            // so ranked results keep working commands above the roadmap.
            .then_with(|| a.0.soon.cmp(&b.0.soon))
            .then_with(|| a.0.name.len().cmp(&b.0.name.len()))
            .then_with(|| a.0.name.cmp(b.0.name))
    });
    out
}

/// When `name` (with leading `/`) is not a known command, try to find the
/// nearest registered command via fuzzy match. Returns `Some(name)` when the
/// top hit passes the quality floor (start-of-name or very strong match).
/// Returns `None` when nothing is close enough — the caller should fall back
/// to a plain "unknown command" message.
pub fn nearest(name: &str) -> Option<&'static str> {
    let query = name.trim_start_matches('/');
    filter(query)
        .first()
        .filter(|(_, s)| *s >= 10)
        .map(|(c, _)| c.name)
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
        assert_eq!(
            ranked[0].0.name, "/model",
            "\"mod\" should rank /model first"
        );
    }

    #[test]
    fn login_is_registered_and_log_ranks_it_first() {
        assert!(
            is_command("/login"),
            "/login should be a live slash command"
        );

        let ranked = filter("log");

        assert_eq!(
            ranked.first().map(|(cmd, _)| cmd.name),
            Some("/login"),
            "\"log\" should rank /login first"
        );
        assert_eq!(
            ranked.first().map(|(cmd, _)| cmd.soon),
            Some(false),
            "/login should be live, not a roadmap placeholder"
        );
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
    fn bare_menu_keeps_groups_contiguous() {
        // The bare `/` menu renders as group SECTIONS, so every group's
        // commands must be contiguous in registry order (a group can't appear,
        // yield to another, then resume — that would split a section header).
        // NOTE: "all live before all soon" is NOT an invariant anymore — a live
        // command (`/memory`) legitimately lives in the roadmap-tier `agent`
        // group. The menu is grouped by topic; live-before-soon is a *ranked*
        // property, enforced by `filter`'s sort and the ranked tests.
        let bare = filter("");
        let mut seen = std::collections::HashSet::new();
        let mut last = "";
        for (c, _) in &bare {
            if c.group != last {
                assert!(
                    seen.insert(c.group),
                    "group {:?} is not contiguous in the bare menu",
                    c.group
                );
                last = c.group;
            }
        }
    }

    #[test]
    fn ranked_results_rank_live_ahead_of_soon_on_ties() {
        // "co" matches live /copy + /compact and soon /context. A working
        // command must lead the ranked list.
        let ranked = filter("co");
        let first_live = ranked.iter().position(|(c, _)| !c.soon);
        let first_soon = ranked.iter().position(|(c, _)| c.soon);
        if let (Some(fl), Some(fs)) = (first_live, first_soon) {
            assert!(fl < fs, "a soon command outranked a live one for 'co'");
        }
    }

    #[test]
    fn is_command_recognizes_registry_only() {
        assert!(is_command("/model"));
        assert!(is_command("/permissions"));
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

    #[test]
    fn nearest_typo_suggests_close_match() {
        // Typing "/provder" (missing 'i' in /providers) should still suggest.
        assert_eq!(nearest("/provder"), Some("/providers"));
    }

    #[test]
    fn nearest_nonsense_returns_none() {
        // "/notacommand" is not a subsequence of any command name, so no
        // near-match should fire — the caller tells the user it's unknown.
        assert_eq!(nearest("/notacommand"), None);
    }

    #[test]
    fn nearest_partial_match_suggests_top_hit() {
        // "/prov" — prefix of /providers, strong enough to suggest.
        assert_eq!(nearest("/prov"), Some("/providers"));
    }

    #[test]
    fn nearest_short_scattered_returns_none() {
        // "/zzz" — not a subsequence of anything, filter empty → None.
        assert_eq!(nearest("/zzz"), None);
    }

    #[test]
    fn nearest_empty_query_strips_slash_and_returns_none() {
        // Bare "/" — filter matches everything (score 0), below threshold → None.
        assert_eq!(nearest("/"), None);
    }
}
