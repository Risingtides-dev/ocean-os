//! Per-turn Longhouse advisory preparation and model-facing presentation.
//!
//! This private leaf owns only the fresh opt-out gate, deterministic rendering
//! and application, and cached read-only preparation under the existing blocking
//! deadline. HTTP adapters, caller orchestration, permissions, runtime authority,
//! events, persistence, governance, calls, and librarian fetch remain outside.

use std::env;

/// Whether the **Longhouse pre-turn consult** is enabled. **Default ON**
/// (OCEAN-283): now that the skill index is cached and the ranking is relevant,
/// the consult-before-acting loop runs for every turn unless an operator opts
/// OUT — the value of consulting the hive before acting only lands if it ships on
/// by default.
///
/// Gated by `OCEAN_LONGHOUSE_PREPARE`:
/// * **unset** → ON (the new default),
/// * an explicit OFF spelling (`0` / `false` / `no` / `off`) → disabled: the turn
///   behaves exactly as before, no skill index loaded, no brief injected, the
///   prompt the model sees byte-for-byte unchanged,
/// * any other / ON spelling (`1` / `true` / `yes` / `on`) → ON.
///
/// History: OCEAN-245 (#168) shipped this hook **default-OFF** behind the same
/// env var, so the prep-loop shipped zero behavior unless opted in. OCEAN-281
/// (#191) made selection cheap + relevant (the skill-librarian `SkillIndex`), and
/// OCEAN-283 caches that index + improves the ranking, so the cost/benefit now
/// favors default-on. The flip is **safe** because the consult stays:
///   * **advisory-only** — the brief is injected into prompt context, never
///     bypasses a permission gate or executes anything (see [`apply_longhouse_prep`]);
///   * **fail-open** — any error / empty / slow path collapses to "no brief" and
///     the turn proceeds with the unmodified prompt, never blocked;
///   * **off the hot path + time-bounded** — the disk scan is cached (no per-turn
///     walk) and the whole prep is wrapped in a deadline (see
///     [`longhouse_prep_for_turn`]), so a slow/missing library can't tax a turn.
///
/// Read fresh per turn (not cached), like the YOLO env resolver, so an operator can
/// flip it by restarting with a different env and tests can scope it.
pub(super) fn longhouse_prepare_enabled() -> bool {
    match env::var("OCEAN_LONGHOUSE_PREPARE") {
        // Explicit opt-OUT only. Everything else (including unset, handled by the
        // Err arm, and any unrecognized value) leaves the default-on consult ON.
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        // Unset → ON (the OCEAN-283 default).
        Err(_) => true,
    }
}

// ---- Longhouse pre-turn consult (default-on, OCEAN-283) -----------------------
//
// PR #159 shipped `POST /v1/longhouse/prepare`; OCEAN-245 (#168) wired it into the
// turn path behind `OCEAN_LONGHOUSE_PREPARE` **default-OFF**, so the
// consult-before-acting loop shipped zero behavior unless opted in. OCEAN-283
// promotes it to **default-ON**: before the LLM call, UNLESS an operator opts out,
// the daemon ranks the turn's prompt against the (cached) local skill libraries
// and injects the resulting compact briefs into prompt context — exactly like
// room/operator guidance (OCEAN-143), prepended so it precedes the task without
// mutating it.
//
// Default-on raises the bar: every turn now consults, so the safety + perf
// invariants below become load-bearing rather than nice-to-haves.
//
// Invariants, straight from `docs/LONGHOUSE.md` (esp. line 115):
//   * **Advisory only.** The brief is a RECOMMENDATION the model reads. It does
//     NOT touch a permission gate, does NOT execute anything, does NOT alter tool
//     routing — `apply_longhouse_prep` only prepends text to the prompt string.
//   * **Fail-open.** A missing/garbled library, an empty index, an irrelevant
//     prompt, a panicked scan, OR a prep that blows its deadline all collapse to
//     "no brief" → the turn proceeds with the unmodified prompt. The prep step can
//     never block a turn.
//   * **Off the hot path + time-bounded.** The skill index is CACHED (no per-turn
//     disk walk — OCEAN-283); any cold/stale reload runs on `spawn_blocking` and
//     under a hard deadline (`LONGHOUSE_PREP_DEADLINE`), so a slow/missing library
//     can never add latency to a turn even though every turn now consults.
//
// The opt-OUT: `OCEAN_LONGHOUSE_PREPARE=0|false|no|off` makes
// `longhouse_prepare_enabled()` false, so none of this runs and the prompt is
// byte-for-byte unchanged — the exact pre-OCEAN-283 behavior, on demand.

/// Render a [`ocean_longhouse::TurnPrep`] into a compact, model-facing context
/// block — or `None` when there is nothing to inject (the fail-open / no-op case).
///
/// Each surfaced skill becomes one bullet: `name — one-line when-to-use`. The
/// header frames it as an **advisory** recommendation, not an instruction or a
/// granted capability, so the model treats it as "here are skills that might be
/// relevant; you still route every action through the normal gates." SOPs and
/// workflows are included on the same footing for forward-compat (always empty in
/// phase 1, so they contribute nothing today).
///
/// Pure + deterministic (no env, no disk): this is the unit-testable core of the
/// injection. The empty-prep case returns `None`, mirroring `render_turn_guidance`.
pub(super) fn render_longhouse_prep(prep: &ocean_longhouse::TurnPrep) -> Option<String> {
    if prep.is_empty() {
        return None;
    }
    let mut block = String::from(
        "Longhouse consult (advisory — relevant skills/SOPs for this turn; \
         recommendations only, not granted capabilities; you still route every \
         action through the normal permission gates):",
    );
    for skill in &prep.skills {
        block.push_str("\n- ");
        block.push_str(&skill.name);
        let desc = skill.description.trim();
        if !desc.is_empty() {
            block.push_str(" — ");
            block.push_str(desc);
        }
    }
    for sop in &prep.sops {
        block.push_str("\n- ");
        block.push_str(&sop.name);
        let desc = sop.description.trim();
        if !desc.is_empty() {
            block.push_str(" — ");
            block.push_str(desc);
        }
    }
    for wf in &prep.workflows {
        block.push_str("\n- ");
        block.push_str(&wf.name);
        let desc = wf.description.trim();
        if !desc.is_empty() {
            block.push_str(" — ");
            block.push_str(desc);
        }
    }
    Some(block)
}

/// Layer a Longhouse consult brief on top of an already-composed turn prompt.
///
/// `prep` is `None` (consult disabled / errored) or an empty [`ocean_longhouse::TurnPrep`]
/// → the prompt is returned **unchanged**, byte-for-byte. A non-empty prep is
/// rendered by [`render_longhouse_prep`] and **prepended** (advisory block, blank
/// line, then the prompt), exactly how room/operator guidance is layered
/// (`apply_turn_guidance`) — steering context precedes the task, the task text is
/// never mutated.
///
/// This is the pure seam the turn-hook test drives: feed it a known prep and
/// assert the brief reaches the prompt; feed it `None` and assert the prompt is
/// untouched. No env, no disk, no async.
pub(super) fn apply_longhouse_prep(
    prompt: &str,
    prep: Option<&ocean_longhouse::TurnPrep>,
) -> String {
    match prep.and_then(render_longhouse_prep) {
        Some(block) => format!("{block}\n\n{prompt}"),
        None => prompt.to_string(),
    }
}

/// Hard deadline for the whole pre-turn consult. Default-on means EVERY turn
/// runs this, so a pathologically slow skill library (a huge tree, a stalled
/// network mount under `~/.spawner`) must never add unbounded latency to a turn.
/// The cache makes the steady state a couple of string scans, but the *first*
/// load after a cold start / TTL expiry still walks disk; this caps even that.
/// On timeout we fail open — inject nothing, let the turn proceed immediately.
pub(super) const LONGHOUSE_PREP_DEADLINE: std::time::Duration =
    std::time::Duration::from_millis(250);

/// The async, env-gated, off-hot-path side of the consult: when the consult is
/// enabled ([`longhouse_prepare_enabled`], **default on** — OCEAN-283), get the
/// **cached** local skill index and rank it against `prompt`, returning the
/// compact [`ocean_longhouse::TurnPrep`] to inject. Returns `None` (inject
/// nothing) when the consult is opted out, the prompt is empty, the result is
/// empty, the scan task fails, or the prep exceeds [`LONGHOUSE_PREP_DEADLINE`] —
/// every one of which is a **fail-open** no-op that leaves the turn exactly as it
/// was without the hook. A consult can never block or slow a turn.
///
/// Two load-bearing perf guarantees, both now that default-on means every turn
/// consults:
/// * **No per-turn disk walk.** The index comes from
///   [`ocean_longhouse::cached_index_for`] — a process-wide TTL cache — so the
///   skill-library walk (`~/.spawner/skills`, `~/.codex/skills`, plus the turn's
///   repo-local `./skills` when `cwd` is set) happens at most once per TTL window
///   per root-set, not once per turn. The steady-state cost is ranking an
///   already-loaded `Vec<SkillBrief>`.
/// * **Time-bounded.** The whole consult (the cache check + any cold/stale
///   reload + the rank) runs on `spawn_blocking` (filesystem I/O must never
///   run on the async scheduler) AND under a [`LONGHOUSE_PREP_DEADLINE`] — if
///   it overruns, we abandon it and inject nothing. So even a first cold load
///   against a degraded disk caps the latency it can add to a turn.
///
/// This performs no side effects and touches no permission gate: it reads the
/// cached index and ranks it, nothing more.
pub(super) async fn longhouse_prep_for_turn(
    prompt: String,
    cwd: String,
) -> Option<ocean_longhouse::TurnPrep> {
    if !longhouse_prepare_enabled() {
        return None;
    }
    // Empty prompt can never rank anything; skip the work entirely.
    if prompt.trim().is_empty() {
        return None;
    }

    let scan = tokio::task::spawn_blocking(move || {
        let roots = if cwd.is_empty() {
            ocean_longhouse::SkillRoots::default()
        } else {
            ocean_longhouse::SkillRoots::for_cwd(&cwd)
        };
        // Cached: at most one disk walk per TTL window per root-set, NOT per turn.
        let index = ocean_longhouse::cached_index_for(&roots);
        let brief = ocean_longhouse::TurnBrief {
            prompt,
            cwd: Some(cwd),
            ..Default::default()
        };
        index.prepare(&brief)
    });

    // Time-bound the whole consult so default-on can never tax a turn: if the
    // (rare) cold/stale reload is slow, we abandon it and inject nothing.
    let prep = match tokio::time::timeout(LONGHOUSE_PREP_DEADLINE, scan).await {
        Ok(joined) => joined,
        Err(_elapsed) => {
            tracing::warn!(
                deadline_ms = LONGHOUSE_PREP_DEADLINE.as_millis() as u64,
                "longhouse pre-turn consult exceeded its deadline; injecting no brief"
            );
            // Dropping the join handle does not cancel this read-only task. It
            // cannot grant authority, but a stalled cold/stale load can retain
            // the process-wide cache lock and make later blocking tasks queue
            // behind it. The current turn still proceeds with no brief; bounding
            // detached work is a separate availability hardening checkpoint.
            return None;
        }
    };

    match prep {
        // Empty prep → inject nothing (no library on disk, or nothing matched).
        Ok(prep) if !prep.is_empty() => Some(prep),
        Ok(_) => None,
        Err(err) => {
            // spawn_blocking only errors on a panic; the loader/ranker don't
            // panic, but stay fail-open regardless — a turn never fails on prep.
            tracing::warn!(error = %err, "longhouse pre-turn consult task failed; injecting no brief");
            None
        }
    }
}
