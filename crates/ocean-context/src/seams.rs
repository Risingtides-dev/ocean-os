//! The four trait seams — THE architectural bet. Layer A declares them with
//! trivial stubs; Layer B (tree-sitter, velocity decay, BM25+embeddings,
//! feature-borrowing) implements them behind the exact same signatures.

use crate::claim::{Anchor, Claim, Velocity};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pseudo-commit meaning "resolve against the working tree, not git history".
pub const WORKTREE: &str = "WORKTREE";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Resolution {
    /// Anchor resolves; payload is S(c) ∈ [0,1] — structural reproducibility.
    Resolves(f32),
    Stale,
    Renamed,
    Dead,
    /// This resolver CANNOT check the anchor (no file anchor, path outside the
    /// repo). Not evidence of anything — "can't check" must never be conflated
    /// with `Stale` or `Dead` (schema friction #2).
    Unresolvable,
}

/// Outcome of computing a write-time baseline (signature/shape hash) for an
/// anchor at its claim's anchor commit. `Option<String>` would be too coarse:
/// "this resolver doesn't do baselines" (file-exists), "the anchor was never
/// attestable at birth" and "the birth revision doesn't parse" demand three
/// different verdicts downstream — collapsing them either changes
/// file-exists behavior or lets a never-attestable claim verify by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Baseline {
    /// This resolver does not compute baselines (the default). `reverify`
    /// proceeds exactly as before — no behavior change for v1 resolvers.
    Unsupported,
    /// Baseline computed: stamp it on the anchor, then resolve normally.
    Stamped(String),
    /// The anchor was NOT attestable at its own anchor commit (file or
    /// symbol absent at birth) — the claim was never reproducible.
    Unattestable,
    /// The birth revision doesn't parse: no evidence either way.
    Unparseable,
}

/// Does this claim's anchor still resolve? v1 stub: file-exists.
/// Layer B (B1): tree-sitter AST + signature hash.
pub trait Resolver {
    fn resolve(&self, anchor: &Anchor, at_commit: &str) -> Resolution;

    /// Write-time baseline for `anchor` as of `at_commit` (the claim's
    /// anchor commit). Default: [`Baseline::Unsupported`] — the resolver has
    /// no notion of shape, and `reverify` must not alter its semantics.
    /// B1 overrides this so the FIRST reverify stamps baselines on anchors
    /// that arrived without one (everything `extract_claims` produces).
    fn baseline_at(&self, _anchor: &Anchor, _at_commit: &str) -> Baseline {
        Baseline::Unsupported
    }
}

pub struct TrustContext {
    /// Unix seconds — always passed in, never `now()` inside the lib.
    pub now: i64,
    /// The owning handoff's `written_at` — the freshness fallback for claims
    /// whose history carries no `written` event (schema friction #6: such
    /// claims must never silently decay from epoch 0).
    pub handoff_written_at: i64,
    /// Semantic/code velocity of the anchored region, measured OUTSIDE the lib
    /// (by [`VelocityMeter`] against real git history) and passed in — the lib
    /// stays `now()`-free and git-free at the trust seam. `None` means "no
    /// velocity measured": [`VelocityDecayTrust`] then falls back to the fixed
    /// half-life, identical to [`ConfidenceRecencyTrust`]. The no-velocity
    /// baseline is the zero case, never a separate code path.
    pub velocity: Option<Velocity>,
}

impl TrustContext {
    /// The no-velocity context (B2 absent): only `now` + the handoff's
    /// `written_at` fallback. [`VelocityDecayTrust`] on this is byte-for-byte
    /// [`ConfidenceRecencyTrust`].
    pub fn new(now: i64, handoff_written_at: i64) -> Self {
        Self {
            now,
            handoff_written_at,
            velocity: None,
        }
    }

    /// Attach a measured velocity (B2). Builder-style so existing call sites
    /// that only know `now`/`written_at` are untouched.
    pub fn with_velocity(mut self, velocity: Velocity) -> Self {
        self.velocity = Some(velocity);
        self
    }
}

/// Score a claim's live trust. v1 stub: confidence × recency.
/// Layer B: the full master equation.
pub trait TrustModel {
    fn trust(&self, claim: &Claim, ctx: &TrustContext) -> f32;
}

/// Rank stored claims by relevance to a query. v1 stub: substring match.
/// Layer B (B3): RRF(bm25, embeddings).
pub trait Retriever {
    fn rank(&self, query: &str, claims: &[Claim]) -> Vec<(usize, f32)>;
}

pub struct Borrowed {
    pub from_id: String,
    pub boost: f32,
}

/// Can a thin claim borrow trust from a richer ancestor? v1 stub: never.
/// Layer B (B5): distributed knowledge over high-PS subspaces.
pub trait Borrower {
    fn borrow(&self, claim: &Claim, candidates: &[Claim]) -> Option<Borrowed>;
}

// ---------------------------------------------------------------------------
// v1 stub implementations
// ---------------------------------------------------------------------------

/// File-exists resolver. Exact path → Resolves(1.0). Basename-only match
/// elsewhere in the tree → Resolves(0.5) (corpus claims often anchor bare
/// filenames like `input.rs`; handoff finding F5). Otherwise Dead.
///
/// Symbol-only anchors (`file: None`, the other half of F5) are Unresolvable:
/// this resolver has no file evidence to attest with until B1's symbol
/// resolution exists. "Can't check" is its own verdict — Stale/Dead would
/// assert evidence we don't have (schema friction #2).
///
/// Anchors that are not repo-relative (absolute paths, `..` segments) take
/// the same Unresolvable arm: `PathBuf::join` would otherwise ignore or
/// escape `repo_root`, verifying claims against files OUTSIDE the repository —
/// breaking distrust-by-default for malformed or untrusted handoffs.
pub struct FileExistsResolver {
    pub repo_root: PathBuf,
}

impl FileExistsResolver {
    fn git(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn basename_match(&self, listing: &str, file: &str) -> bool {
        let needle = format!("/{file}");
        listing.lines().any(|l| l == file || l.ends_with(&needle))
    }
}

/// Lexical repo-relativity check (v1): reject empty paths (not a file this
/// resolver can attest — `join("")` is the repo dir itself), absolute paths
/// and any `..` component. Deliberately NOT filesystem canonicalization —
/// resolving symlinks would itself touch paths outside the repo. A pure
/// component walk is enough to keep `PathBuf::join` from replacing or
/// escaping `repo_root`.
pub(crate) fn is_repo_relative(file: &str) -> bool {
    use std::path::Component;
    let p = std::path::Path::new(file);
    !file.is_empty()
        && !p.is_absolute()
        && p.components().all(|c| {
            !matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

impl Resolver for FileExistsResolver {
    fn resolve(&self, anchor: &Anchor, at_commit: &str) -> Resolution {
        // Symbol-only anchors carry no file evidence — typed absence, no
        // sentinel (schema friction #1). This resolver cannot check them.
        let Some(file) = anchor.file.as_deref() else {
            return Resolution::Unresolvable;
        };
        // Path-traversal gate BEFORE any filesystem/git probe: absolute or
        // `..`-bearing paths would make `join` ignore or escape `repo_root`
        // (and must not be handed to git either). Not checkable by us →
        // Unresolvable, never Stale/Dead (schema friction #2).
        if !is_repo_relative(file) {
            return Resolution::Unresolvable;
        }
        if at_commit == WORKTREE {
            if self.repo_root.join(file).exists() {
                return Resolution::Resolves(1.0);
            }
            if !file.contains('/') {
                if let Some(listing) = self.git(&["ls-files"]) {
                    if self.basename_match(&listing, file) {
                        return Resolution::Resolves(0.5);
                    }
                }
            }
            return Resolution::Dead;
        }
        let spec = format!("{at_commit}:{file}");
        if self.git(&["cat-file", "-e", &spec]).is_some() {
            return Resolution::Resolves(1.0);
        }
        if !file.contains('/') {
            if let Some(listing) = self.git(&["ls-tree", "-r", "--name-only", at_commit]) {
                if self.basename_match(&listing, file) {
                    return Resolution::Resolves(0.5);
                }
            }
        }
        Resolution::Dead
    }
}

/// Write-time confidence × exponential recency decay, fixed 30-day half-life.
pub struct ConfidenceRecencyTrust {
    pub half_life_secs: f64,
}

impl Default for ConfidenceRecencyTrust {
    fn default() -> Self {
        Self {
            half_life_secs: 30.0 * 24.0 * 3600.0,
        }
    }
}

impl TrustModel for ConfidenceRecencyTrust {
    fn trust(&self, claim: &Claim, ctx: &TrustContext) -> f32 {
        // A claim with no `written` history event dates from when its handoff
        // was written — never from epoch 0, never "infinitely fresh"
        // (schema friction #6).
        let written = claim.written_at().unwrap_or(ctx.handoff_written_at);
        let dt = (ctx.now - written).max(0) as f64;
        let decay = (-(std::f64::consts::LN_2) * dt / self.half_life_secs).exp();
        claim.confidence * decay as f32
    }
}

/// B2 — the velocity-modulated forgetting term of the master equation
/// (OCEAN-313): `e^(−λ(v)·Δt)` with `λ(v) = ln2/H`, `H = H₀·e^(−κ·v_sem)`.
///
/// **What it does.** The half-life is no longer fixed. It *contracts with
/// churn*: high `v_sem` (molten code) → small `H` → fast decay (everything
/// provisional, reverify cheap); `v_sem ≈ 0` (frozen code) → `H ≈ H₀` → slow
/// decay, so a broken anchor on a crystallized region is a *real alarm* rather
/// than expected drift. The system tapers itself — it reads whether the
/// codebase's representational geometry has stiffened and lengthens trust
/// accordingly. `v_sem` is the disentanglement-rate proxy from
/// [`VelocityMeter`] (see its docs for why churn-rate is a faithful stand-in
/// for the spec's representation-kernel `d/dt` at this layer).
///
/// **Reduces to the baseline.** With no measured velocity
/// (`ctx.velocity == None`) OR `v_sem == 0`, `H = H₀` and this is *exactly*
/// [`ConfidenceRecencyTrust`] — same `confidence × e^(−ln2·Δt/H₀)`, bit-for-bit
/// (`reduces_to_confidence_recency_when_no_velocity` asserts it). B2 adds a
/// term, it does not replace the v1 curve; the v1 model stays as the
/// no-velocity baseline.
///
/// **Constants, derived against real ocean-os history (not guessed).** Measured
/// at HEAD, 7-day window, with [`VelocityMeter`]'s own `v_sem` formula:
/// - `h0_secs = 30 days` — matches the v1 fixed half-life, so the `v_sem = 0`
///   reduction is to the *same* number, not a near-miss.
/// - `kappa = ln2 / v_inflect`, `v_inflect = 5.27` (the measured `v_sem` of a
///   mature-but-not-dead provider, `crates/ocean-protocol/src/providers/
///   anthropic.rs`) ⟹ `κ ≈ 0.1315`. This puts the half-life inflection
///   (`H = H₀/2`, one extra halving of trust from velocity alone) exactly at
///   the maturity boundary: a region as live as a built-out provider gets half
///   the frozen half-life. Measured taper at HEAD — frozen anchor (`input.rs`,
///   built-then-abandoned, `v_sem≈0`) keeps the full 30 days; frozen doc
///   (`README.md`, 0.42) → 28.4 days; mature provider (5.27) → 15 days (the
///   inflection); warm protocol (`core/lib.rs`, 10.4) → 7.7 days; hot session
///   layer (`agent/lib.rs`, 21.9) → 1.7 days; molten hub (`daemon/main.rs`,
///   61.5) → ~13 minutes. Whether that maturity inflection is reasonable is a
///   human judgment call — see the PR's taper-curve table.
pub struct VelocityDecayTrust {
    /// Frozen-anchor half-life in seconds (`H₀`). Default 30 days — the v1
    /// fixed half-life, so `v_sem = 0` reduces to [`ConfidenceRecencyTrust`].
    pub h0_secs: f64,
    /// Stiffening coefficient `κ`. Default `≈0.1315` (`ln2 / 5.27`): the
    /// half-life halves at `v_sem ≈ 5.27`, the measured churn rate of a mature
    /// ocean-os provider file.
    pub kappa: f64,
}

impl Default for VelocityDecayTrust {
    fn default() -> Self {
        // κ = ln2 / v_inflect, v_inflect = 5.27 (measured anthropic.rs v_sem).
        Self {
            h0_secs: 30.0 * 24.0 * 3600.0,
            kappa: std::f64::consts::LN_2 / 5.27,
        }
    }
}

impl VelocityDecayTrust {
    /// The velocity-modulated half-life `H = H₀·e^(−κ·v_sem)` in seconds.
    /// `v_sem` is clamped to be non-negative — a churn *rate* is never
    /// negative, and a stray negative would *lengthen* the half-life past
    /// frozen, which is meaningless.
    pub fn half_life_secs(&self, v_sem: f32) -> f64 {
        let v = f64::from(v_sem).max(0.0);
        self.h0_secs * (-self.kappa * v).exp()
    }
}

impl TrustModel for VelocityDecayTrust {
    fn trust(&self, claim: &Claim, ctx: &TrustContext) -> f32 {
        // Same freshness rule as v1 (schema friction #6): no `written` event
        // ⇒ date from the handoff's `written_at`, never epoch 0.
        let written = claim.written_at().unwrap_or(ctx.handoff_written_at);
        let dt = (ctx.now - written).max(0) as f64;
        // No measured velocity ⇒ v_sem = 0 ⇒ H = H₀: the baseline curve.
        let v_sem = ctx.velocity.map_or(0.0, |v| v.v_sem);
        let h = self.half_life_secs(v_sem);
        let decay = (-(std::f64::consts::LN_2) * dt / h).exp();
        claim.confidence * decay as f32
    }
}

/// Case-insensitive word-overlap scoring; zero-score claims are dropped.
pub struct SubstringRetriever;

impl Retriever for SubstringRetriever {
    fn rank(&self, query: &str, claims: &[Claim]) -> Vec<(usize, f32)> {
        let words: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if words.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(usize, f32)> = claims
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let text = c.text.to_lowercase();
                let hits = words.iter().filter(|w| text.contains(w.as_str())).count();
                (hits > 0).then_some((i, hits as f32 / words.len() as f32))
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored
    }
}

/// Never borrows. Layer B replaces this with distributed-knowledge borrowing.
pub struct NoBorrow;

impl Borrower for NoBorrow {
    fn borrow(&self, _claim: &Claim, _candidates: &[Claim]) -> Option<Borrowed> {
        None
    }
}

// ---------------------------------------------------------------------------
// B2 — velocity measurement from real git history (OCEAN-313)
// ---------------------------------------------------------------------------

/// Default trailing window for the churn-rate proxy, in days. Chosen against
/// ocean-os's ~36-day history: a window this short reads the *instantaneous*
/// local churn rate (a file built-then-abandoned reads ~0; a constantly-edited
/// hub reads high), whereas a 30-day window on a 36-day repo just accumulates
/// cumulative age and can't separate molten from frozen. Configurable.
pub const VELOCITY_WINDOW_DAYS: i64 = 7;

/// Measures a claim's anchor velocity from REAL git history — the input to
/// [`VelocityDecayTrust`]'s `v_sem`. Lives OUTSIDE the trust seam (it touches
/// git; the lib's `trust()` does not), so the trust math stays pure and
/// `now()`-free: you measure here, pass the [`Velocity`] in via
/// [`TrustContext::with_velocity`].
///
/// ## The proxy (and why it is faithful — the TRUE kernel `d/dt` is DEFERRED)
///
/// The spec defines `v_sem` as the *rate of disentanglement* — the time
/// derivative of the representation-kernel geometry (Wang–Fusi). Computing that
/// literally needs embeddings + the parallelism-score machinery (B4), which do
/// not exist yet and must not be pulled forward (staging discipline). So B2
/// ships a **churn-rate proxy**: the rate at which a claim's anchor file(s)
/// are being rewritten in git, measured over a trailing window ending at the
/// claim's anchor commit.
///
/// Why churn-rate is a faithful stand-in *at this layer*: the representation
/// kernel only disentangles when the *code changes* — a file no commit touches
/// cannot be moving through representational space, and a file under constant
/// rewrite is precisely one whose structure has not yet crystallized. Commit
/// frequency on the anchored region is therefore a monotone, observable lower
/// bound on the true `d/dt`: it cannot see *semantic* churn inside a
/// byte-identical file (a dependency's meaning shifting underneath it), so it
/// under-reports, never over-reports, disentanglement — the safe direction
/// (it errs toward *longer* trust, never spuriously shorter). It is exact at
/// the two poles the taper cares about (frozen ⇒ 0, molten ⇒ large), and
/// `VelocityDecayTrust` consumes only `v_sem`'s magnitude, not its provenance —
/// so B4 can later swap the kernel `d/dt` in behind this same `Velocity`
/// without touching the trust model.
///
/// ## Trust boundary (mirrors B1's `TreeSitterResolver` exactly)
///
/// - git reads only via `git -C <repo_root>` — **never a checkout**;
/// - `--follow` so a renamed anchor's history isn't truncated at the rename;
/// - anchors that are not [`is_repo_relative`] (absolute, `..`) contribute
///   nothing — `Velocity` stays `0` for them, never an error and never a git
///   probe on an out-of-tree path;
/// - a symlinked anchor path is skipped (consistent with B1's symlink trust
///   boundary): its history is the link's, not the region the claim is about.
pub struct VelocityMeter {
    pub repo_root: PathBuf,
    /// Trailing window in days (default [`VELOCITY_WINDOW_DAYS`]).
    pub window_days: i64,
}

impl VelocityMeter {
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            window_days: VELOCITY_WINDOW_DAYS,
        }
    }

    fn git(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Author time (unix seconds) of `rev`, or `None` if it can't be read
    /// (unknown/pruned commit in a shallow clone — uncheckable, never an
    /// error). `%at` (author), NOT `%ct` (committer): a rebased / cherry-picked
    /// / imported history rewrites committer dates to the rewrite time, which
    /// would make old churn look recent and prematurely shorten trust. The
    /// window end uses this, and the in-window filter below uses `%at` too, so
    /// both bounds are in the same (author) time base. Erring toward intrinsic
    /// authoring time keeps trust longer under history rewrites — the safe
    /// direction.
    fn commit_time(&self, rev: &str) -> Option<i64> {
        self.git(&["log", "-1", "--format=%at", rev])
            .and_then(|s| s.trim().parse::<i64>().ok())
    }

    /// Whether the in-repo `file` path crosses a symlink at the working tree
    /// (the same one-segment `symlink_metadata` walk B1 uses). At-history reads
    /// can't follow a symlink — git tracks the link blob — but a working-tree
    /// symlink whose *string* is repo-relative still points out of the trust
    /// boundary, so its commit history is not the anchored region's. Callers
    /// pass only `is_repo_relative` paths, so the accumulated path can't escape
    /// `repo_root` during the walk.
    fn crosses_symlink(&self, file: &str) -> bool {
        let mut acc = self.repo_root.clone();
        for comp in Path::new(file).components() {
            if let std::path::Component::Normal(seg) = comp {
                acc.push(seg);
                match std::fs::symlink_metadata(&acc) {
                    Ok(md) if md.file_type().is_symlink() => return true,
                    Ok(_) => {}
                    Err(_) => return false, // vanished mid-walk: nothing to follow
                }
            }
        }
        false
    }

    /// Whether `file` is a REGULAR tracked blob (mode 100644 / 100755) at
    /// `rev`. `ls-tree` lists nothing for an absent path and reports the mode
    /// for a present one, so this is existence + mode in one probe: a symlink
    /// blob (120000) or a tree (040000) is rejected — only a real file carries
    /// a churn signal. The path is forced to a literal pathspec so anchor magic
    /// can't widen the match.
    fn is_regular_blob_at(&self, file: &str, rev: &str) -> bool {
        let literal = format!(":(literal){file}");
        let Some(out) = self.git(&["ls-tree", "-z", rev, "--", &literal]) else {
            return false;
        };
        // `ls-tree` rows are `<mode> SP <type> SP <object> TAB <path>`; with
        // `-z` they are NUL-separated. The mode is the leading whitespace-
        // delimited token of each row.
        out.split('\0')
            .filter(|row| !row.is_empty())
            .filter_map(|row| row.split_whitespace().next())
            .any(|mode| mode == "100644" || mode == "100755")
    }

    /// The repo-relative path whose churn should be measured for anchor `file`,
    /// or `None` when the anchor carries no attributable velocity signal.
    ///
    /// - Exact path present as a regular file → itself.
    /// - Bare basename (no `/`) absent at the exact path but matching exactly
    ///   ONE regular file elsewhere → that file (B1's corpus basename
    ///   fallback: `input.rs` anchors a nested `crates/.../input.rs`). A basename
    ///   that matches zero or MULTIPLE files yields `None` — churn can't be
    ///   attributed to one of several candidates (stricter than B1's weak
    ///   `Resolves(0.5)`, because averaging churn across files is meaningless).
    /// - Symlinks, directories, out-of-tree, deleted-at-anchor → `None`.
    ///
    /// `at_commit == WORKTREE` resolves against the live checkout; otherwise
    /// against the tree at `at_commit` (object reads, no filesystem traversal).
    fn effective_path(&self, file: &str, at_commit: &str, time_rev: &str) -> Option<String> {
        if at_commit == WORKTREE {
            // Live filesystem: the same symlink guard B1 applies in WORKTREE
            // mode (a symlinked path points out of the trust boundary).
            if self.crosses_symlink(file) {
                return None;
            }
            // Exact path present as a regular file (not a symlink/dir/missing —
            // a locally-deleted file still in HEAD must NOT borrow HEAD churn).
            if matches!(
                std::fs::symlink_metadata(self.repo_root.join(file)),
                Ok(md) if md.file_type().is_file()
            ) {
                return Some(file.to_string());
            }
            // Bare-basename fallback: unique tracked match in the working tree
            // that ALSO exists on disk as a regular file. `ls-files` reflects
            // the index, which still lists a locally-deleted file — so the
            // candidate must be re-checked on disk, or a deleted bare-basename
            // anchor would borrow HEAD churn (regressing the deletion gate).
            if !file.contains('/') {
                if let Some(listing) = self.git(&["ls-files"]) {
                    if let Some(cand) = self.unique_basename(&listing, file) {
                        if matches!(
                            std::fs::symlink_metadata(self.repo_root.join(&cand)),
                            Ok(md) if md.file_type().is_file()
                        ) {
                            return Some(cand);
                        }
                    }
                }
            }
            None
        } else {
            // At-commit: accept only a REGULAR tracked blob. `ls-tree` lists
            // nothing for an absent path and reports the mode for a present
            // one, so existence + mode in one probe — a symlink blob (120000)
            // or tree (040000) would otherwise have its blob/edits counted as
            // churn (the symlink trust boundary in the past tense).
            if self.is_regular_blob_at(file, at_commit) {
                return Some(file.to_string());
            }
            // Bare-basename fallback against the tree: unique regular-file
            // match. `-r` recurses; rows are `<mode> <type> <obj>\t<path>`.
            if !file.contains('/') {
                if let Some(listing) = self.git(&["ls-tree", "-r", time_rev]) {
                    let regulars: String = listing
                        .lines()
                        .filter(|row| {
                            let mode = row.split_whitespace().next().unwrap_or("");
                            mode == "100644" || mode == "100755"
                        })
                        .filter_map(|row| row.split('\t').nth(1))
                        .collect::<Vec<_>>()
                        .join("\n");
                    return self.unique_basename(&regulars, file);
                }
            }
            None
        }
    }

    /// The single listing path whose basename equals `file`, or `None` if zero
    /// or more than one match (ambiguous location → no attributable churn).
    fn unique_basename(&self, listing: &str, file: &str) -> Option<String> {
        let needle = format!("/{file}");
        let mut hits = listing
            .lines()
            .filter(|l| *l == file || l.ends_with(&needle));
        let first = hits.next()?;
        match hits.next() {
            None => Some(first.to_string()), // exactly one
            Some(_) => None,                 // ambiguous
        }
    }

    /// Raw counts for one anchor file over the trailing window ending at
    /// `at_commit`: `Some((resolved_path, commits, churn_lines))` where
    /// `churn_lines` is summed added+deleted across those commits, and
    /// `resolved_path` is the path actually measured (the exact anchor or its
    /// unique basename resolution) so the caller can DEDUP by it — two anchors
    /// naming the same file differently must not double-count. `--follow` keeps
    /// a rename's earlier history attached. `None` for an unattestable anchor
    /// (out of tree, symlinked, ambiguous basename, unreadable commit, or
    /// absent AT the anchor commit) — silence, never error.
    fn counts_for_file(&self, file: &str, at_commit: &str) -> Option<(String, u32, u64)> {
        if !is_repo_relative(file) {
            return None;
        }
        // The trailing window's end time: a real commit's timestamp, or HEAD's
        // for a working-tree measurement (WORKTREE is not a revision, so it
        // can't date the window directly).
        let time_rev = if at_commit == WORKTREE {
            "HEAD"
        } else {
            at_commit
        };
        // Resolve the path whose churn we measure: the exact anchor when it is
        // a present regular file, else the UNIQUE bare-basename match (B1's
        // corpus fallback). Ambiguous / absent / non-regular → no signal.
        let path = self.effective_path(file, at_commit, time_rev)?;
        let end = self.commit_time(time_rev)?;
        let lo = end - self.window_days * 86_400;
        // `--` separates options from paths but does NOT disable pathspec
        // magic: an untrusted anchor like `:(glob)**/*.rs` would otherwise
        // glob the whole repo and inflate velocity, prematurely decaying
        // trust. Force the (resolved) path to a LITERAL pathspec so git takes
        // it verbatim. `:(literal)` is a single pathspec, which `--follow`
        // requires.
        let literal = format!(":(literal){path}");
        // Walk from `time_rev` (HEAD for a working-tree measurement — WORKTREE
        // is not a revision; committed history is what `git log` can read).
        let out = self.git(&[
            "log",
            "--follow",
            // `%at` (author time), matching `commit_time`'s window bounds — a
            // rewritten committer date must not pull old churn into the window.
            "--format=COMMIT %at",
            "--numstat",
            time_rev,
            "--",
            &literal,
        ])?;
        let mut commits: u32 = 0;
        let mut churn: u64 = 0;
        let mut in_window = false;
        for line in out.lines() {
            if let Some(rest) = line.strip_prefix("COMMIT ") {
                let t: i64 = rest.trim().parse().unwrap_or(i64::MIN);
                in_window = t >= lo && t <= end;
                if in_window {
                    commits += 1;
                }
            } else if in_window && !line.is_empty() {
                // numstat: "<added>\t<deleted>\t<path>"; binary files show "-".
                let mut it = line.split('\t');
                if let (Some(a), Some(d)) = (it.next(), it.next()) {
                    churn += a.parse::<u64>().unwrap_or(0) + d.parse::<u64>().unwrap_or(0);
                }
            }
        }
        Some((path, commits, churn))
    }

    /// Measure `(v_code, v_sem)` for a claim's anchor set at its anchor commit.
    ///
    /// Both axes are **rates per day** over the trailing window, aggregated
    /// across the claim's anchor files (a claim touching several hot files is
    /// more molten than one touching a single hot file). To stay robust to a
    /// claim that anchors the same file twice, files are de-duplicated first.
    ///
    /// - `v_code` = commits/day (structural-edit frequency on the region);
    /// - `v_sem` = a churn-magnitude-weighted rate, `commits/day · (1 + ln(1 +
    ///   lines_per_commit))`. Pure commit-frequency (`v_code`) treats a
    ///   one-line tweak and a full rewrite alike; weighting by *how much* each
    ///   commit moved (sub-linear, so a giant mechanical reformat can't
    ///   dominate) tracks disentanglement more closely — a region rewritten in
    ///   large strokes is less crystallized than one receiving tiny touches at
    ///   the same cadence. With zero commits both are exactly `0.0`, so a
    ///   frozen anchor reduces [`VelocityDecayTrust`] to the baseline.
    pub fn measure(&self, claim: &Claim) -> Velocity {
        let commit = claim.provenance.commit_sha.as_str();
        if commit.is_empty() {
            return Velocity {
                v_code: 0.0,
                v_sem: 0.0,
            };
        }
        // Dedup by RESOLVED path, not by the raw anchor string: two anchors
        // that name the same file differently (bare `input.rs` + full
        // `crates/.../input.rs`) resolve to one tracked path and must be
        // counted once, or velocity would be inflated for multi-anchor claims
        // that point at a single nested file. Each unique resolved path
        // contributes its counts exactly once.
        let mut seen: std::collections::HashMap<String, (u32, u64)> =
            std::collections::HashMap::new();
        for anchor in &claim.provenance.anchors {
            let Some(file) = anchor.file.as_deref() else {
                continue;
            };
            if let Some((path, c, churn)) = self.counts_for_file(file, commit) {
                seen.entry(path).or_insert((c, churn));
            }
        }
        let mut total_commits: u32 = 0;
        let mut total_churn: u64 = 0;
        for (c, churn) in seen.values() {
            total_commits += c;
            total_churn += churn;
        }
        let window = self.window_days.max(1) as f32;
        let v_code = total_commits as f32 / window;
        let v_sem = if total_commits == 0 {
            0.0
        } else {
            let lines_per_commit = total_churn as f32 / total_commits as f32;
            v_code * (1.0 + (1.0 + lines_per_commit).ln())
        };
        Velocity { v_code, v_sem }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::tests::sample_handoff;

    fn ctx_at(now: i64) -> TrustContext {
        TrustContext::new(now, 1_780_980_000)
    }

    #[test]
    fn trust_decays_with_age_and_scales_with_confidence() {
        let h = sample_handoff();
        let claim = &h.claims[0]; // confidence 0.9, written at 1_780_980_000
        let trust = ConfidenceRecencyTrust::default();
        let fresh = trust.trust(claim, &ctx_at(1_780_980_000));
        assert!((fresh - 0.9).abs() < 1e-6);
        // one half-life (30 days) later → half the trust
        let later = trust.trust(claim, &ctx_at(1_780_980_000 + 30 * 24 * 3600));
        assert!((later - 0.45).abs() < 1e-3);
    }

    /// Schema friction #6: a claim whose history has no `written` event dates
    /// from the handoff's written_at — it neither decays from epoch 0 (~zero
    /// trust) nor passes as infinitely fresh.
    #[test]
    fn empty_history_falls_back_to_handoff_written_at() {
        let h = sample_handoff();
        let mut claim = h.claims[0].clone(); // confidence 0.9
        claim.history.clear();
        let trust = ConfidenceRecencyTrust::default();
        let one_half_life = ctx_at(1_780_980_000 + 30 * 24 * 3600);
        let got = trust.trust(&claim, &one_half_life);
        assert!(
            (got - 0.45).abs() < 1e-3,
            "decays from handoff written_at, got {got}"
        );
    }

    // ---- B2: velocity-modulated decay (OCEAN-313) ----

    /// THE reduction contract (spec deliverable #1): `VelocityDecayTrust` with
    /// no measured velocity must be byte-for-byte `ConfidenceRecencyTrust`.
    /// Same H₀ on both sides, swept across the whole decay curve.
    #[test]
    fn reduces_to_confidence_recency_when_no_velocity() {
        let h = sample_handoff();
        let claim = &h.claims[0];
        let baseline = ConfidenceRecencyTrust::default();
        let b2 = VelocityDecayTrust::default(); // h0 == baseline half-life
        let t0 = 1_780_980_000;
        for days in [0, 1, 7, 15, 30, 60, 120, 365] {
            let ctx = ctx_at(t0 + days * 24 * 3600); // velocity: None
            let a = baseline.trust(claim, &ctx);
            let b = b2.trust(claim, &ctx);
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "diverged at day {days}: {a} vs {b}"
            );
        }
    }

    /// v_sem == 0 (an explicitly-measured frozen anchor) is the same zero case
    /// as `velocity: None`: still exactly the baseline.
    #[test]
    fn v_sem_zero_is_the_baseline() {
        let h = sample_handoff();
        let claim = &h.claims[0];
        let baseline = ConfidenceRecencyTrust::default();
        let b2 = VelocityDecayTrust::default();
        let ctx = ctx_at(1_780_980_000 + 30 * 24 * 3600).with_velocity(Velocity {
            v_code: 0.0,
            v_sem: 0.0,
        });
        assert_eq!(
            baseline.trust(claim, &ctx).to_bits(),
            b2.trust(claim, &ctx).to_bits()
        );
    }

    /// The taper: higher churn ⇒ shorter half-life ⇒ less surviving trust at a
    /// fixed Δt. Monotone strictly decreasing in v_sem.
    #[test]
    fn higher_velocity_decays_faster() {
        let h = sample_handoff();
        let claim = &h.claims[0];
        let b2 = VelocityDecayTrust::default();
        let dt_ctx = |v_sem: f32| {
            ctx_at(1_780_980_000 + 7 * 24 * 3600).with_velocity(Velocity { v_code: 0.0, v_sem })
        };
        let frozen = b2.trust(claim, &dt_ctx(0.0));
        let mature = b2.trust(claim, &dt_ctx(5.27));
        let warm = b2.trust(claim, &dt_ctx(10.4));
        let molten = b2.trust(claim, &dt_ctx(61.5));
        assert!(frozen > mature, "{frozen} !> {mature}");
        assert!(mature > warm, "{mature} !> {warm}");
        assert!(warm > molten, "{warm} !> {molten}");
        // Frozen at 7 days < one 30-day half-life ⇒ still most of the trust.
        assert!(frozen > 0.5 * claim.confidence);
        // Molten anchor is near-worthless after a week.
        assert!(molten < 0.02);
    }

    /// The half-life formula itself: H(0) = H₀, and the inflection H₀/2 lands
    /// at the derived v_sem (≈5.27, the mature-provider rate).
    #[test]
    fn half_life_inflection_lands_at_mature_velocity() {
        let b2 = VelocityDecayTrust::default();
        let h0 = b2.h0_secs;
        assert!((b2.half_life_secs(0.0) - h0).abs() < 1e-6);
        // κ = ln2/5.27 ⇒ H(5.27) = H₀·e^(−ln2) = H₀/2 (within f64 round-trip).
        assert!((b2.half_life_secs(5.27) - h0 / 2.0).abs() < h0 * 1e-6);
        // A negative rate is impossible; it must not lengthen H past frozen.
        assert!(b2.half_life_secs(-3.0) <= h0);
    }

    /// A negative `now - written` (clock skew / claim from the future) must not
    /// blow trust above confidence — same `.max(0)` guard as the baseline.
    #[test]
    fn future_dated_claim_does_not_exceed_confidence() {
        let h = sample_handoff();
        let claim = &h.claims[0];
        let b2 = VelocityDecayTrust::default();
        let past = ctx_at(1_780_980_000 - 10 * 24 * 3600).with_velocity(Velocity {
            v_code: 1.0,
            v_sem: 5.0,
        });
        assert!((b2.trust(claim, &past) - claim.confidence).abs() < 1e-6);
    }

    #[test]
    fn substring_retriever_ranks_matching_claims_first_and_drops_misses() {
        let mut h = sample_handoff();
        let mut other = h.claims[0].clone();
        other.id = "c2".into();
        other.text = "daemon session save race in ocean-agent".into();
        h.claims.push(other);
        let ranked = SubstringRetriever.rank("session race", &h.claims);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0, 1); // index of the matching claim
    }

    #[test]
    fn no_borrow_never_borrows() {
        let h = sample_handoff();
        assert!(NoBorrow.borrow(&h.claims[0], &h.claims).is_none());
    }

    /// Symbol-only anchors (`file: None`, F5) must NOT spuriously resolve in
    /// either mode — and must come back Unresolvable ("can't check"), never
    /// Stale/Dead (schema friction #2).
    #[test]
    fn symbol_only_anchor_is_unresolvable_in_both_modes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "c1"]);
        let head = git(&["rev-parse", "HEAD"]);

        let r = FileExistsResolver {
            repo_root: root.to_path_buf(),
        };
        let symbol_only = Anchor {
            file: None,
            symbol: Some("workspace.members".into()),
            lines: vec![],
            sig_hash: None,
        };
        assert_eq!(r.resolve(&symbol_only, WORKTREE), Resolution::Unresolvable);
        assert_eq!(r.resolve(&symbol_only, &head), Resolution::Unresolvable);
        // A degenerate Some("") is equally uncheckable — `join("")` is the
        // repo dir and `cat-file -e <rev>:` accepts the bare tree-ish.
        let empty_file = Anchor {
            file: Some(String::new()),
            ..symbol_only.clone()
        };
        assert_eq!(r.resolve(&empty_file, WORKTREE), Resolution::Unresolvable);
        assert_eq!(r.resolve(&empty_file, &head), Resolution::Unresolvable);
    }

    /// Anchors outside the repo (absolute path or `..` escape) must be
    /// Unresolvable in BOTH modes even when the outside file genuinely exists —
    /// otherwise `join` ignores/escapes repo_root and we verify foreign files.
    #[test]
    fn outside_repo_anchors_are_unresolvable_in_both_modes() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "c1"]);
        let head = git(&["rev-parse", "HEAD"]);

        // A file that genuinely exists ABOVE the repo root.
        std::fs::write(outer.path().join("escape.rs"), "fn outside() {}\n").unwrap();

        let r = FileExistsResolver {
            repo_root: root.clone(),
        };
        let mk = |file: &str| Anchor {
            file: Some(file.into()),
            symbol: None,
            lines: vec![],
            sig_hash: None,
        };
        let escape = mk("../escape.rs");
        let nested_escape = mk("src/../../escape.rs");
        let absolute = mk("/etc/hosts"); // exists on the host, but not ours to attest
        for anchor in [&escape, &nested_escape, &absolute] {
            let file = anchor.file.as_deref().unwrap();
            assert_eq!(
                r.resolve(anchor, WORKTREE),
                Resolution::Unresolvable,
                "{file}"
            );
            assert_eq!(r.resolve(anchor, &head), Resolution::Unresolvable, "{file}");
        }
        // Sanity: the in-repo anchor still resolves, so the gate is not over-broad.
        assert_eq!(r.resolve(&mk("a.rs"), WORKTREE), Resolution::Resolves(1.0));
        assert_eq!(r.resolve(&mk("./a.rs"), &head), Resolution::Resolves(1.0));
    }

    #[test]
    fn file_exists_resolver_in_worktree_mode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "fn main() {}\n").unwrap();
        let r = FileExistsResolver {
            repo_root: dir.path().to_path_buf(),
        };
        let present = Anchor {
            file: Some("src/a.rs".into()),
            symbol: None,
            lines: vec![],
            sig_hash: None,
        };
        let missing = Anchor {
            file: Some("src/gone.rs".into()),
            symbol: None,
            lines: vec![],
            sig_hash: None,
        };
        assert!(matches!(
            r.resolve(&present, WORKTREE),
            Resolution::Resolves(_)
        ));
        assert!(matches!(r.resolve(&missing, WORKTREE), Resolution::Dead));
    }
}
