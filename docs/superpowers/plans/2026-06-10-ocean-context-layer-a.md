# ocean-context Layer A Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `crates/ocean-context` Layer A — the handoff doc engine: typed anchored claims, markdown+frontmatter store, four stub trait seams, and a git-history replay binary. Zero LLM calls.

**Architecture:** Library-first crate in the ocean-os workspace. `claim.rs` holds the serde schema; `extract.rs` ports the validated Python regex pass; `store.rs` owns the on-disk markdown+TOML-frontmatter format; `seams.rs` declares the four traits (Resolver/TrustModel/Retriever/Borrower) with trivial stubs; `replay.rs` walks real git history and emits verdicts a human judges. The replay harness IS the production reverification core.

**Tech Stack:** Rust 2021, serde, toml 0.8, regex, clap (bin), anyhow. Git operations shell out to the `git` CLI (no git2). Tests use tempfile for throwaway git repos.

**Source of truth:** `docs/specs/ocean-context-handoff-engine.md`. The trait signatures in Task 6 are copied from it verbatim — they are THE architectural bet; flag any deviation loudly.

**F1 resolution (decided):** Layer A's `Resolver` stub stays file-exists (with a basename fallback so corpus claims that anchor bare filenames like `input.rs` still resolve). Tree-sitter is B1, built first against this seam — the trait boundary stays honest.

**Repo conventions:** ocean-os is GitButler-managed. Use `but` for all write operations (`but status --json` to get file IDs, `but commit <branch> -m "…" --changes <ids>`). Never `git commit`/`git checkout`. Read-only git (`git log`, `git diff`) is fine.

**Regression corpus (verified 2026-06-10):** the Python prototype at
`/Users/risingtidesdev/dev/claude-monorepo/.superpowers/brainstorm/sim/extract_claims.py`
extracts exactly **22** claims from `ocean-os/HANDOFF.md` and **29** from
`claude-monorepo/docs/PHASE2_HANDOFF.md` (= the 51-claim corpus). Fixtures are snapshotted in Task 3 so the numbers stay frozen.

---

### Task 1: Branch, commit the specs, scaffold the crate

**Files:**
- Create: `crates/ocean-context/Cargo.toml`
- Create: `crates/ocean-context/src/lib.rs` (skeleton)
- Modify: `Cargo.toml` (workspace members, default-members, workspace.dependencies)

- [ ] **Step 1: Create the stack**

```bash
cd /Users/risingtidesdev/dev/ocean-os
but status --json
but branch new ocean-context-layer-a
```

- [ ] **Step 2: Commit the spec + handoff docs (currently untracked)**

```bash
but status --json   # get CLI IDs for docs/specs/ocean-context-handoff-engine.md and docs/specs/HANDOFF-ocean-context.md and this plan file
but commit ocean-context-layer-a -m "docs: ocean-context spec, hand-authored handoff, Layer A plan" --changes <ids>
```

Commit ONLY the two `docs/specs/*.md` files and `docs/superpowers/plans/2026-06-10-ocean-context-layer-a.md`. Do NOT commit `.playwright-mcp/`, `docs/orchestrator/`, or `gpui_masterbuild.md` — unrelated strays.

- [ ] **Step 3: Add `regex` to workspace dependencies**

In root `Cargo.toml` `[workspace.dependencies]`, after the `toml = "0.8"` line add:

```toml
regex = "1"
ocean-context = { path = "crates/ocean-context" }
```

- [ ] **Step 4: Add the crate to `members` AND `default-members`**

Add `"crates/ocean-context",` to both lists (after `"crates/ocean-plugin",` in each).

- [ ] **Step 5: Create `crates/ocean-context/Cargo.toml`**

```toml
[package]
name = "ocean-context"
version = "0.1.0"
edition.workspace = true
license.workspace = true
authors.workspace = true
rust-version.workspace = true

[dependencies]
anyhow.workspace = true
clap.workspace = true
regex.workspace = true
serde.workspace = true
toml.workspace = true

[dev-dependencies]
serde_json.workspace = true
tempfile = "3"

[[bin]]
name = "ocean-context-replay"
path = "src/bin/replay.rs"
```

- [ ] **Step 6: Create skeleton `src/lib.rs`** (modules land in later tasks)

```rust
//! ocean-context — the handoff as a context primitive.
//!
//! A handoff is a set of claims, each with provenance, that the receiving
//! session distrusts by default and reverifies against ground truth.
//! Spec: docs/specs/ocean-context-handoff-engine.md
```

Also create `src/bin/replay.rs` as a placeholder so the bin target compiles:

```rust
fn main() {
    eprintln!("replay: not implemented yet");
}
```

- [ ] **Step 7: Verify the workspace builds**

Run: `cargo build -p ocean-context`
Expected: compiles clean.

- [ ] **Step 8: Commit**

```bash
but status --json
but commit ocean-context-layer-a -m "feat(ocean-context): scaffold crate in workspace" --changes <ids>
```

---

### Task 2: claim.rs — the serde schema

**Files:**
- Create: `crates/ocean-context/src/claim.rs`
- Modify: `crates/ocean-context/src/lib.rs`

- [ ] **Step 1: Write the failing test** — append to the bottom of `src/claim.rs` (create the file with ONLY the test module + `use` lines for now if you want strict TDD, or write schema+test together and watch the test compile-fail first):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample_handoff() -> Handoff {
        Handoff {
            session_id: "sess-1".into(),
            parent_session: None,
            repo: "ocean-os".into(),
            branch: "main".into(),
            commit_anchor: "d9a9bc9".into(),
            scope_ring: ScopeRing::Repo,
            velocity_at_write: Velocity { v_code: 0.0, v_sem: 0.0 },
            written_at: 1_780_980_000,
            narrative: "The prose handoff.\n".into(),
            claims: vec![Claim {
                id: "c1".into(),
                text: "mutators implement requires_permission".into(),
                provenance: Provenance {
                    anchors: vec![Anchor {
                        file: "crates/ocean-runtime/src/tools/browser/input.rs".into(),
                        symbol: Some("requires_permission".into()),
                        lines: vec![29, 67, 97, 130],
                        sig_hash: None,
                    }],
                    ticket: Some("OCEAN-16".into()),
                    commit_sha: "d9a9bc9".into(),
                },
                status: ClaimStatus::Verified,
                knowledge_tier: KnowledgeTier::Individual,
                ps_anchor: None,
                confidence: 0.9,
                borrowed_from: None,
                history: vec![ClaimEvent {
                    at: 1_780_980_000,
                    event: "written".into(),
                    by_session: "sess-1".into(),
                }],
            }],
        }
    }

    #[test]
    fn json_round_trip_is_lossless() {
        let h = sample_handoff();
        let json = serde_json::to_string(&h).unwrap();
        let back: Handoff = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn claim_written_at_reads_history() {
        let h = sample_handoff();
        assert_eq!(h.claims[0].written_at(), Some(1_780_980_000));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ocean-context claim`
Expected: compile error — types not defined.

- [ ] **Step 3: Write the schema** (top of `src/claim.rs`, exactly the spec's structs):

```rust
//! The codified handoff schema. Human prose stays in `narrative`;
//! the machine-checkable substance is `claims`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Handoff {
    pub session_id: String,
    pub parent_session: Option<String>,
    pub repo: String,
    pub branch: String,
    /// The clock claims are dated against (short sha).
    pub commit_anchor: String,
    pub scope_ring: ScopeRing,
    /// v1: zeros. Layer B fills these.
    pub velocity_at_write: Velocity,
    /// Unix seconds — always passed in, never `now()` inside the lib.
    pub written_at: i64,
    pub narrative: String,
    pub claims: Vec<Claim>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Claim {
    pub id: String,
    pub text: String,
    pub provenance: Provenance,
    pub status: ClaimStatus,
    /// v1: defaults to Individual. Layer B computes it.
    pub knowledge_tier: KnowledgeTier,
    /// Layer B (Wang-Fusi parallelism score). None in v1.
    pub ps_anchor: Option<f32>,
    /// Trust at write-time.
    pub confidence: f32,
    /// Distributed-knowledge edge (Layer B).
    pub borrowed_from: Option<String>,
    /// written | reverified | promoted | killed — self-versioning.
    pub history: Vec<ClaimEvent>,
}

impl Claim {
    /// Unix time of the original `written` event, if recorded.
    pub fn written_at(&self) -> Option<i64> {
        self.history.iter().find(|e| e.event == "written").map(|e| e.at)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Provenance {
    /// What reverification re-resolves.
    pub anchors: Vec<Anchor>,
    pub ticket: Option<String>,
    pub commit_sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Anchor {
    pub file: String,
    pub symbol: Option<String>,
    /// May be empty — symbol-only and file-only anchors are common (handoff finding F5).
    pub lines: Vec<u32>,
    /// Layer B (tree-sitter signature hash). None in v1.
    pub sig_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClaimStatus {
    Verified,
    Reverify,
    Stale,
    Dead,
    Asserted,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum KnowledgeTier {
    Common,
    Individual,
    Distributed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ScopeRing {
    Session,
    Branch,
    Repo,
    Brain,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Velocity {
    pub v_code: f32,
    pub v_sem: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClaimEvent {
    pub at: i64,
    pub event: String,
    pub by_session: String,
}
```

- [ ] **Step 4: Wire the module** — in `src/lib.rs` add:

```rust
pub mod claim;

pub use claim::*;
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p ocean-context claim`
Expected: 2 passed.

- [ ] **Step 6: Commit**

```bash
but status --json
but commit ocean-context-layer-a -m "feat(ocean-context): claim schema (serde structs from spec)" --changes <ids>
```

---

### Task 3: Snapshot the regression fixtures

**Files:**
- Create: `crates/ocean-context/tests/fixtures/ocean-os-HANDOFF.md`
- Create: `crates/ocean-context/tests/fixtures/claude-monorepo-PHASE2_HANDOFF.md`

- [ ] **Step 1: Copy the two real corpus docs as frozen fixtures**

```bash
mkdir -p crates/ocean-context/tests/fixtures
cp /Users/risingtidesdev/dev/ocean-os/HANDOFF.md \
   crates/ocean-context/tests/fixtures/ocean-os-HANDOFF.md
cp /Users/risingtidesdev/dev/claude-monorepo/docs/PHASE2_HANDOFF.md \
   crates/ocean-context/tests/fixtures/claude-monorepo-PHASE2_HANDOFF.md
```

- [ ] **Step 2: Sanity-check the snapshot against the Python prototype**

```bash
python3 - <<'EOF'
import sys
sys.path.insert(0, "/Users/risingtidesdev/dev/claude-monorepo/.superpowers/brainstorm/sim")
from extract_claims import extract
for p in ["crates/ocean-context/tests/fixtures/ocean-os-HANDOFF.md",
          "crates/ocean-context/tests/fixtures/claude-monorepo-PHASE2_HANDOFF.md"]:
    _, claims = extract(p)
    print(p, len(claims))
EOF
```

Expected: `22` and `29`. If they differ, the live docs drifted since 2026-06-10 — use whatever the prototype prints NOW as the regression numbers in Task 4 and note it in the final report.

- [ ] **Step 3: Commit**

```bash
but status --json
but commit ocean-context-layer-a -m "test(ocean-context): freeze 51-claim regression corpus" --changes <ids>
```

---

### Task 4: extract.rs — port the regex anchor pass

**Files:**
- Create: `crates/ocean-context/src/extract.rs`
- Create: `crates/ocean-context/tests/extract_regression.rs`
- Modify: `crates/ocean-context/src/lib.rs`

The Python reference is `/Users/risingtidesdev/dev/claude-monorepo/.superpowers/brainstorm/sim/extract_claims.py`. Port it faithfully — the regression numbers depend on it. Key semantics to preserve:
- A header line (starts with `#`) sets `in_verified` from `(?i)(verified|ground truth|already done|current state|don.?t re-?verify)` — and RESETS it to false on every header that doesn't match.
- Each line is trimmed of ` \t-*•` from both ends; lines under 12 chars are skipped.
- Only lines with ≥1 file anchor become claims.
- Line lists parse `29,67,97,130` and ranges `37–82`/`37-82` (en-dash normalized) into start+end points.
- Text truncates to 280 chars; symbols cap at 6.

- [ ] **Step 1: Write the failing tests** — `tests/extract_regression.rs`:

```rust
use ocean_context::claim::ClaimStatus;
use ocean_context::extract::{extract_claims, ExtractCtx};

fn ctx() -> ExtractCtx<'static> {
    ExtractCtx { commit_sha: "d9a9bc9", now: 1_780_980_000, by_session: "regression-test" }
}

#[test]
fn ocean_os_handoff_yields_22_claims() {
    let text = include_str!("fixtures/ocean-os-HANDOFF.md");
    assert_eq!(extract_claims(text, &ctx()).len(), 22);
}

#[test]
fn phase2_handoff_yields_29_claims() {
    let text = include_str!("fixtures/claude-monorepo-PHASE2_HANDOFF.md");
    assert_eq!(extract_claims(text, &ctx()).len(), 29);
}

#[test]
fn input_rs_anchor_parses_line_list_and_verified_section() {
    let text = include_str!("fixtures/ocean-os-HANDOFF.md");
    let claims = extract_claims(text, &ctx());
    // The "Verified ground truth" section anchors `input.rs:29,67,97,130`.
    let c = claims
        .iter()
        .find(|c| c.provenance.anchors.iter().any(|a| a.file == "input.rs"))
        .expect("input.rs claim present");
    let a = c.provenance.anchors.iter().find(|a| a.file == "input.rs").unwrap();
    assert_eq!(a.lines, vec![29, 67, 97, 130]);
    assert_eq!(c.status, ClaimStatus::Verified);
}

#[test]
fn unanchored_lines_are_skipped() {
    let claims = extract_claims("This long sentence mentions no file anchors at all.", &ctx());
    assert!(claims.is_empty());
}

#[test]
fn range_lines_normalize_en_dash() {
    let claims = extract_claims("Single browser + single active page: lib.rs:37–82 holds it.", &ctx());
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].provenance.anchors[0].lines, vec![37, 82]);
}

#[test]
fn ticket_and_symbol_are_captured() {
    let claims =
        extract_claims("Phase 1 done (OCEAN-16): `append_client_type` arm in crates/ocean-agent/src/lib.rs.", &ctx());
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].provenance.ticket.as_deref(), Some("OCEAN-16"));
    assert_eq!(claims[0].provenance.anchors[0].symbol.as_deref(), Some("append_client_type"));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ocean-context --test extract_regression`
Expected: compile error — `extract` module not defined.

- [ ] **Step 3: Implement `src/extract.rs`**

```rust
//! Pass-1 claim extraction: deterministically pull anchored claims out of
//! prose HANDOFF.md docs. Zero LLM. Faithful port of the validated Python
//! prototype (51-claim corpus); the regression tests freeze its behavior.

use crate::claim::{Anchor, Claim, ClaimEvent, ClaimStatus, KnowledgeTier, Provenance};
use regex::Regex;
use std::sync::OnceLock;

pub struct ExtractCtx<'a> {
    /// Commit the claims are dated against.
    pub commit_sha: &'a str,
    /// Unix seconds for the `written` history event.
    pub now: i64,
    pub by_session: &'a str,
}

fn anchor_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"([A-Za-z0-9_./-]+\.(?:rs|ts|tsx|js|jsx|py|go|toml|md|sql|json))(?::(\d+(?:[,\-–]\d+)*))?",
        )
        .expect("anchor regex")
    })
}

fn ticket_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b([A-Z]{2,}-\d+)\b").expect("ticket regex"))
}

fn symbol_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"`([a-zA-Z_][a-zA-Z0-9_]*(?:::[a-zA-Z0-9_]+)*)\(?\)?`").expect("symbol regex")
    })
}

fn verified_hdr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(verified|ground truth|already done|current state|don.?t re-?verify)")
            .expect("verified header regex")
    })
}

/// v1 confidence is DERIVED, never free-typed (handoff finding F3):
/// base by section, small bump per extra anchor.
fn derive_confidence(anchor_count: usize, declared_verified: bool) -> f32 {
    let base = if declared_verified { 0.8 } else { 0.5 };
    (base + 0.05 * anchor_count.min(4) as f32).min(1.0)
}

pub fn extract_claims(text: &str, ctx: &ExtractCtx) -> Vec<Claim> {
    let mut claims = Vec::new();
    let mut in_verified = false;
    for raw in text.lines() {
        if raw.starts_with('#') {
            in_verified = verified_hdr_re().is_match(raw);
        }
        let l = raw
            .trim_matches(|c: char| matches!(c, ' ' | '\t' | '-' | '*' | '•'))
            .trim();
        if l.chars().count() < 12 {
            continue;
        }
        let mut anchors = Vec::new();
        for cap in anchor_re().captures_iter(l) {
            let mut lines = Vec::new();
            if let Some(ls) = cap.get(2) {
                for part in ls.as_str().split(',') {
                    let part = part.replace('–', "-");
                    if let Some((a, b)) = part.split_once('-') {
                        if let Ok(n) = a.parse::<u32>() {
                            lines.push(n);
                        }
                        if let Ok(n) = b.parse::<u32>() {
                            lines.push(n);
                        }
                    } else if let Ok(n) = part.parse::<u32>() {
                        lines.push(n);
                    }
                }
            }
            anchors.push(Anchor { file: cap[1].to_string(), symbol: None, lines, sig_hash: None });
        }
        if anchors.is_empty() {
            continue; // pass-1: only structurally-anchored claims
        }
        // v1 heuristic: pair the i-th backticked symbol with the i-th anchor.
        let symbols: Vec<String> =
            symbol_re().captures_iter(l).map(|c| c[1].to_string()).take(6).collect();
        for (anchor, sym) in anchors.iter_mut().zip(symbols.iter()) {
            anchor.symbol = Some(sym.clone());
        }
        let ticket = ticket_re().captures(l).map(|c| c[1].to_string());
        let confidence = derive_confidence(anchors.len(), in_verified);
        claims.push(Claim {
            id: format!("c{}", claims.len() + 1),
            text: l.chars().take(280).collect(),
            provenance: Provenance { anchors, ticket, commit_sha: ctx.commit_sha.to_string() },
            status: if in_verified { ClaimStatus::Verified } else { ClaimStatus::Asserted },
            knowledge_tier: KnowledgeTier::Individual,
            ps_anchor: None,
            confidence,
            borrowed_from: None,
            history: vec![ClaimEvent {
                at: ctx.now,
                event: "written".to_string(),
                by_session: ctx.by_session.to_string(),
            }],
        });
    }
    claims
}
```

- [ ] **Step 4: Wire the module** — in `src/lib.rs` add `pub mod extract;`

- [ ] **Step 5: Run the regression tests**

Run: `cargo test -p ocean-context --test extract_regression`
Expected: 6 passed. **If a count test fails:** run the Python prototype on the same fixture and diff the claim texts against the Rust output to find the divergent line — fix the port, not the number.

- [ ] **Step 6: Commit**

```bash
but status --json
but commit ocean-context-layer-a -m "feat(ocean-context): regex claim extraction, regression-locked to 51-claim corpus" --changes <ids>
```

---

### Task 5: store.rs — markdown + TOML frontmatter on disk

**Files:**
- Create: `crates/ocean-context/src/store.rs`
- Modify: `crates/ocean-context/src/lib.rs`

Format decision (resolves handoff finding F4): the machine-owned part (metadata + claims) is **TOML frontmatter** between `+++` delimiters (toml 0.8 already a workspace dep; serializer is order-safe); the human narrative is the markdown body. `store.rs` owns this format — humans never hand-edit claims.

- [ ] **Step 1: Write the failing tests** — test module at the bottom of `src/store.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::tests::sample_handoff;

    #[test]
    fn markdown_round_trip_is_lossless() {
        let h = sample_handoff();
        let md = to_markdown(&h).unwrap();
        assert!(md.starts_with("+++\n"));
        let back = from_markdown(&md).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn write_then_read_freshest_picks_latest_for_repo_and_branch() {
        let dir = tempfile::tempdir().unwrap();
        let mut old = sample_handoff();
        old.session_id = "sess-old".into();
        old.written_at = 100;
        let mut new = sample_handoff();
        new.session_id = "sess-new".into();
        new.written_at = 200;
        let mut other_branch = sample_handoff();
        other_branch.session_id = "sess-other".into();
        other_branch.branch = "feature/x".into();
        other_branch.written_at = 300;

        write_handoff(dir.path(), &old).unwrap();
        write_handoff(dir.path(), &new).unwrap();
        write_handoff(dir.path(), &other_branch).unwrap();

        let got = read_freshest(dir.path(), "ocean-os", "main").unwrap().unwrap();
        assert_eq!(got.session_id, "sess-new");
        assert!(read_freshest(dir.path(), "ocean-os", "nope").unwrap().is_none());
    }
}
```

NOTE: this borrows `sample_handoff()` from claim.rs's test module — change its declaration in `claim.rs` from `mod tests` private fn to `pub(crate) fn sample_handoff()` inside `#[cfg(test)] pub(crate) mod tests` (i.e. make the module `pub(crate)`).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ocean-context store`
Expected: compile error.

- [ ] **Step 3: Implement `src/store.rs`**

```rust
//! On-disk codified handoffs: TOML frontmatter (machine-owned) + markdown
//! narrative (human-owned), one file per handoff. Layer B may move this to
//! pg/graph behind the same functions.

use crate::claim::{Claim, Handoff, ScopeRing, Velocity};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const DELIM: &str = "+++";

/// Everything except the narrative lives in frontmatter.
#[derive(Serialize, Deserialize)]
struct FrontMatter {
    session_id: String,
    parent_session: Option<String>,
    repo: String,
    branch: String,
    commit_anchor: String,
    scope_ring: ScopeRing,
    velocity_at_write: Velocity,
    written_at: i64,
    claims: Vec<Claim>,
}

pub fn to_markdown(h: &Handoff) -> Result<String> {
    let fm = FrontMatter {
        session_id: h.session_id.clone(),
        parent_session: h.parent_session.clone(),
        repo: h.repo.clone(),
        branch: h.branch.clone(),
        commit_anchor: h.commit_anchor.clone(),
        scope_ring: h.scope_ring,
        velocity_at_write: h.velocity_at_write,
        written_at: h.written_at,
        claims: h.claims.clone(),
    };
    let toml = toml::to_string(&fm).context("serializing handoff frontmatter")?;
    Ok(format!("{DELIM}\n{toml}{DELIM}\n\n{}", h.narrative))
}

pub fn from_markdown(text: &str) -> Result<Handoff> {
    let rest = text.strip_prefix(DELIM).context("missing opening +++ frontmatter delimiter")?;
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let (fm_str, body) =
        rest.split_once("\n+++").context("missing closing +++ frontmatter delimiter")?;
    let fm: FrontMatter = toml::from_str(fm_str).context("parsing handoff frontmatter")?;
    Ok(Handoff {
        session_id: fm.session_id,
        parent_session: fm.parent_session,
        repo: fm.repo,
        branch: fm.branch,
        commit_anchor: fm.commit_anchor,
        scope_ring: fm.scope_ring,
        velocity_at_write: fm.velocity_at_write,
        written_at: fm.written_at,
        narrative: body.trim_start_matches('\n').to_string(),
        claims: fm.claims,
    })
}

fn file_name(h: &Handoff) -> String {
    let safe: String = h
        .session_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    format!("{}-{}.handoff.md", h.written_at, safe)
}

/// Write a codified handoff into `dir`. Returns the path written.
pub fn write_handoff(dir: &Path, h: &Handoff) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(file_name(h));
    fs::write(&path, to_markdown(h)?).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Most recent handoff for (repo, branch) in `dir`, by `written_at`.
/// Unparseable files are skipped (warned to stderr), not fatal.
pub fn read_freshest(dir: &Path, repo: &str, branch: &str) -> Result<Option<Handoff>> {
    let mut best: Option<Handoff> = None;
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(None), // no handoff dir yet
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.file_name().is_some_and(|n| n.to_string_lossy().ends_with(".handoff.md")) {
            continue;
        }
        let text = fs::read_to_string(&path)?;
        match from_markdown(&text) {
            Ok(h) if h.repo == repo && h.branch == branch => {
                if best.as_ref().is_none_or(|b| h.written_at > b.written_at) {
                    best = Some(h);
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("ocean-context: skipping unparseable {}: {e}", path.display()),
        }
    }
    Ok(best)
}
```

If `is_none_or` isn't available on the toolchain, use `best.as_ref().map_or(true, |b| h.written_at > b.written_at)`.

- [ ] **Step 4: Wire the module** — in `src/lib.rs` add `pub mod store;`. In `claim.rs` make the test module `pub(crate)` and `sample_handoff` `pub(crate)` as noted.

- [ ] **Step 5: Run tests**

Run: `cargo test -p ocean-context`
Expected: all green (claim + extract + store).

- [ ] **Step 6: Commit**

```bash
but status --json
but commit ocean-context-layer-a -m "feat(ocean-context): markdown+TOML-frontmatter handoff store" --changes <ids>
```

---

### Task 6: seams.rs — the four traits + v1 stubs (THE BET)

**Files:**
- Create: `crates/ocean-context/src/seams.rs`
- Modify: `crates/ocean-context/src/lib.rs`

The trait signatures below are verbatim from the spec. **Do not "improve" them.** Layer B swaps real organs in behind these exact traits.

- [ ] **Step 1: Write the failing tests** — test module at the bottom of `src/seams.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::tests::sample_handoff;

    #[test]
    fn trust_decays_with_age_and_scales_with_confidence() {
        let h = sample_handoff();
        let claim = &h.claims[0]; // confidence 0.9, written at 1_780_980_000
        let trust = ConfidenceRecencyTrust::default();
        let fresh = trust.trust(claim, &TrustContext { now: 1_780_980_000 });
        assert!((fresh - 0.9).abs() < 1e-6);
        // one half-life (30 days) later → half the trust
        let later = trust.trust(claim, &TrustContext { now: 1_780_980_000 + 30 * 24 * 3600 });
        assert!((later - 0.45).abs() < 1e-3);
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

    #[test]
    fn file_exists_resolver_in_worktree_mode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "fn main() {}\n").unwrap();
        let r = FileExistsResolver { repo_root: dir.path().to_path_buf() };
        let present = Anchor { file: "src/a.rs".into(), symbol: None, lines: vec![], sig_hash: None };
        let missing = Anchor { file: "src/gone.rs".into(), symbol: None, lines: vec![], sig_hash: None };
        assert!(matches!(r.resolve(&present, WORKTREE), Resolution::Resolves(_)));
        assert!(matches!(r.resolve(&missing, WORKTREE), Resolution::Dead));
    }
}
```

(Git-mode resolution is covered by the replay integration test in Task 8.)

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ocean-context seams`
Expected: compile error.

- [ ] **Step 3: Implement `src/seams.rs`**

```rust
//! The four trait seams — THE architectural bet. Layer A declares them with
//! trivial stubs; Layer B (tree-sitter, velocity decay, BM25+embeddings,
//! feature-borrowing) implements them behind the exact same signatures.

use crate::claim::{Anchor, Claim};
use std::path::PathBuf;
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
}

/// Does this claim's anchor still resolve? v1 stub: file-exists.
/// Layer B (B1): tree-sitter AST + signature hash.
pub trait Resolver {
    fn resolve(&self, anchor: &Anchor, at_commit: &str) -> Resolution;
}

pub struct TrustContext {
    /// Unix seconds — always passed in, never `now()` inside the lib.
    pub now: i64,
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
pub struct FileExistsResolver {
    pub repo_root: PathBuf,
}

impl FileExistsResolver {
    fn git(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("git").arg("-C").arg(&self.repo_root).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn basename_match(&self, listing: &str, file: &str) -> bool {
        let needle = format!("/{file}");
        listing.lines().any(|l| l == file || l.ends_with(&needle))
    }
}

impl Resolver for FileExistsResolver {
    fn resolve(&self, anchor: &Anchor, at_commit: &str) -> Resolution {
        if at_commit == WORKTREE {
            if self.repo_root.join(&anchor.file).exists() {
                return Resolution::Resolves(1.0);
            }
            if !anchor.file.contains('/') {
                if let Some(listing) = self.git(&["ls-files"]) {
                    if self.basename_match(&listing, &anchor.file) {
                        return Resolution::Resolves(0.5);
                    }
                }
            }
            return Resolution::Dead;
        }
        let spec = format!("{at_commit}:{}", anchor.file);
        if self.git(&["cat-file", "-e", &spec]).is_some() {
            return Resolution::Resolves(1.0);
        }
        if !anchor.file.contains('/') {
            if let Some(listing) = self.git(&["ls-tree", "-r", "--name-only", at_commit]) {
                if self.basename_match(&listing, &anchor.file) {
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
        Self { half_life_secs: 30.0 * 24.0 * 3600.0 }
    }
}

impl TrustModel for ConfidenceRecencyTrust {
    fn trust(&self, claim: &Claim, ctx: &TrustContext) -> f32 {
        let written = claim.written_at().unwrap_or(ctx.now);
        let dt = (ctx.now - written).max(0) as f64;
        let decay = (-(std::f64::consts::LN_2) * dt / self.half_life_secs).exp();
        claim.confidence * decay as f32
    }
}

/// Case-insensitive word-overlap scoring; zero-score claims are dropped.
pub struct SubstringRetriever;

impl Retriever for SubstringRetriever {
    fn rank(&self, query: &str, claims: &[Claim]) -> Vec<(usize, f32)> {
        let words: Vec<String> =
            query.to_lowercase().split_whitespace().map(str::to_string).collect();
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
```

- [ ] **Step 4: Wire the module** — in `src/lib.rs` add `pub mod seams;` and re-export:

```rust
pub use seams::{
    Borrowed, Borrower, ConfidenceRecencyTrust, FileExistsResolver, NoBorrow, Resolution,
    Resolver, Retriever, SubstringRetriever, TrustContext, TrustModel, WORKTREE,
};
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p ocean-context seams`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
but status --json
but commit ocean-context-layer-a -m "feat(ocean-context): four trait seams + v1 stub organs" --changes <ids>
```

---

### Task 7: lib.rs public API — write_handoff / read_freshest / reverify

**Files:**
- Modify: `crates/ocean-context/src/lib.rs`
- Create: `crates/ocean-context/tests/api.rs`

- [ ] **Step 1: Write the failing tests** — `tests/api.rs`:

```rust
use ocean_context::claim::{Anchor, ClaimStatus};
use ocean_context::seams::{FileExistsResolver, Resolution, WORKTREE};
use ocean_context::{extract_claims, ExtractCtx};
use ocean_context::{read_freshest, reverify, write_handoff, Handoff, ScopeRing, Velocity};

fn handoff_with(claim_texts: &[&str]) -> Handoff {
    let prose = claim_texts.join("\n");
    let ctx = ExtractCtx { commit_sha: "abc1234", now: 1_000, by_session: "test" };
    Handoff {
        session_id: "sess-api".into(),
        parent_session: None,
        repo: "ocean-os".into(),
        branch: "main".into(),
        commit_anchor: "abc1234".into(),
        scope_ring: ScopeRing::Repo,
        velocity_at_write: Velocity { v_code: 0.0, v_sem: 0.0 },
        written_at: 1_000,
        narrative: prose.clone(),
        claims: extract_claims(&prose, &ctx),
    }
}

#[test]
fn write_then_read_freshest_sorts_claims_by_trust() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = handoff_with(&[
        "Low-confidence assertion about some/old/path.rs here.",
        "Another assertion touching crates/other/file.rs today.",
    ]);
    // Make claim 2 strictly more trusted.
    h.claims[0].confidence = 0.2;
    h.claims[1].confidence = 0.9;
    write_handoff(dir.path(), &h).unwrap();
    let got = read_freshest(dir.path(), "ocean-os", "main", 1_000).unwrap().unwrap();
    assert_eq!(got.claims[0].confidence, 0.9); // most trusted first
}

#[test]
fn reverify_updates_status_and_history_via_resolver() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/real.rs"), "// here\n").unwrap();

    let mut h = handoff_with(&[
        "A live claim anchored at src/real.rs in this repo.",
        "A dead claim anchored at src/vanished.rs long gone.",
    ]);
    let resolver = FileExistsResolver { repo_root: dir.path().to_path_buf() };
    let results = reverify(&mut h, &resolver, WORKTREE, 2_000, "sess-next");

    assert_eq!(results.len(), 2);
    assert!(matches!(results[0].1, Resolution::Resolves(_)));
    assert!(matches!(results[1].1, Resolution::Dead));
    assert_eq!(h.claims[0].status, ClaimStatus::Verified);
    assert_eq!(h.claims[1].status, ClaimStatus::Dead);
    // history gained a reverified event
    assert!(h.claims[0].history.iter().any(|e| e.event.starts_with("reverified")));
}

#[test]
fn reverify_skips_anchorless_claims() {
    let mut h = handoff_with(&["A live claim anchored at src/real.rs in this repo."]);
    // Hand-build an anchorless asserted claim (e.g. a plan statement).
    let mut plan = h.claims[0].clone();
    plan.id = "c-plan".into();
    plan.provenance.anchors = vec![];
    plan.status = ClaimStatus::Asserted;
    h.claims.push(plan);

    let dir = tempfile::tempdir().unwrap();
    let resolver = FileExistsResolver { repo_root: dir.path().to_path_buf() };
    let results = reverify(&mut h, &resolver, WORKTREE, 2_000, "sess-next");
    assert_eq!(results.len(), 1); // anchorless claim untouched
    assert_eq!(h.claims[1].status, ClaimStatus::Asserted);
}

#[test]
fn anchors_can_be_file_only() {
    // F5: the resolver path must not assume line numbers.
    let a = Anchor { file: "src/real.rs".into(), symbol: None, lines: vec![], sig_hash: None };
    assert!(a.lines.is_empty());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ocean-context --test api`
Expected: compile error — `read_freshest`/`reverify` not defined at crate root.

- [ ] **Step 3: Implement the public API** — replace `src/lib.rs` content with:

```rust
//! ocean-context — the handoff as a context primitive.
//!
//! A handoff is a set of claims, each with provenance, that the receiving
//! session distrusts by default and reverifies against ground truth.
//! Spec: docs/specs/ocean-context-handoff-engine.md

pub mod claim;
pub mod extract;
pub mod replay;
pub mod seams;
pub mod store;

pub use claim::*;
pub use extract::{extract_claims, ExtractCtx};
pub use seams::{
    Borrowed, Borrower, ConfidenceRecencyTrust, FileExistsResolver, NoBorrow, Resolution,
    Resolver, Retriever, SubstringRetriever, TrustContext, TrustModel, WORKTREE,
};

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Write a codified handoff into `dir`. Returns the path written.
pub fn write_handoff(dir: &Path, handoff: &Handoff) -> Result<PathBuf> {
    store::write_handoff(dir, handoff)
}

/// Most recent handoff for (repo, branch), claims sorted most-trusted-first
/// by the stub TrustModel. `now` is unix seconds (passed in, never computed).
pub fn read_freshest(dir: &Path, repo: &str, branch: &str, now: i64) -> Result<Option<Handoff>> {
    let Some(mut h) = store::read_freshest(dir, repo, branch)? else {
        return Ok(None);
    };
    let trust = ConfidenceRecencyTrust::default();
    let ctx = TrustContext { now };
    h.claims.sort_by(|a, b| trust.trust(b, &ctx).total_cmp(&trust.trust(a, &ctx)));
    Ok(Some(h))
}

/// Re-resolve every anchored claim at `at_commit` (or [`WORKTREE`]); update
/// statuses and append history events. Anchorless claims are left untouched —
/// there is nothing to check. Returns (claim id, resolution) per checked claim.
pub fn reverify(
    handoff: &mut Handoff,
    resolver: &dyn Resolver,
    at_commit: &str,
    now: i64,
    by_session: &str,
) -> Vec<(String, Resolution)> {
    fn rank(r: Resolution) -> u8 {
        match r {
            Resolution::Resolves(_) => 3,
            Resolution::Renamed => 2,
            Resolution::Stale => 1,
            Resolution::Dead => 0,
        }
    }
    let mut out = Vec::new();
    for claim in &mut handoff.claims {
        if claim.provenance.anchors.is_empty() {
            continue;
        }
        let mut best = Resolution::Dead;
        for anchor in &claim.provenance.anchors {
            let r = resolver.resolve(anchor, at_commit);
            if rank(r) > rank(best) {
                best = r;
            }
        }
        claim.status = match best {
            Resolution::Resolves(_) => ClaimStatus::Verified,
            Resolution::Renamed => ClaimStatus::Reverify,
            Resolution::Stale => ClaimStatus::Stale,
            Resolution::Dead => ClaimStatus::Dead,
        };
        claim.history.push(ClaimEvent {
            at: now,
            event: format!("reverified:{best:?}"),
            by_session: by_session.to_string(),
        });
        out.push((claim.id.clone(), best));
    }
    out
}
```

NOTE: `pub mod replay;` requires `src/replay.rs` to exist — create it now with just the module doc comment (`//! Replay harness — implemented in the next task.`) so this task compiles standalone.

- [ ] **Step 4: Run tests**

Run: `cargo test -p ocean-context`
Expected: all green.

- [ ] **Step 5: Commit**

```bash
but status --json
but commit ocean-context-layer-a -m "feat(ocean-context): public API — write_handoff, read_freshest, reverify" --changes <ids>
```

---

### Task 8: replay.rs — walk real git history, emit verdicts

**Files:**
- Modify: `crates/ocean-context/src/replay.rs`
- Create: `crates/ocean-context/tests/replay_git.rs`

- [ ] **Step 1: Write the failing integration test** — `tests/replay_git.rs` builds a throwaway git repo:

```rust
use ocean_context::extract::{extract_claims, ExtractCtx};
use ocean_context::replay::replay;
use ocean_context::seams::FileExistsResolver;
use std::path::Path;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn replay_finds_the_commit_where_an_anchor_dies() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);

    std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c1: add a.rs and b.rs"]);
    let c1 = git(root, &["rev-parse", "HEAD"]);

    std::fs::write(root.join("b.rs"), "fn b() { /* changed */ }\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c2: touch b.rs"]);

    git(root, &["rm", "-q", "a.rs"]);
    git(root, &["commit", "-qm", "c3: remove a.rs"]);
    let c3 = git(root, &["rev-parse", "HEAD"]);

    let prose = "The a feature lives in a.rs as designed.\n\
                 The b feature lives in b.rs and is stable.";
    let ctx = ExtractCtx { commit_sha: &c1, now: 0, by_session: "replay-test" };
    let claims = extract_claims(prose, &ctx);
    assert_eq!(claims.len(), 2);

    let resolver = FileExistsResolver { repo_root: root.to_path_buf() };
    let verdicts = replay(root, &claims, &resolver).unwrap();

    assert_eq!(verdicts.len(), 2);
    // a.rs dies at c3
    assert_eq!(verdicts[0].first_fail_commit.as_deref(), Some(c3.as_str()));
    // b.rs survives the whole walk
    assert_eq!(verdicts[1].first_fail_commit, None);
    assert_eq!(verdicts[1].commits_walked, 2); // c2 and c3
}

#[test]
fn replay_notes_unwalkable_claims_instead_of_failing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("x.rs"), "fn x() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "only commit"]);

    let ctx = ExtractCtx { commit_sha: "deadbeef", now: 0, by_session: "replay-test" };
    let claims = extract_claims("Anchored claim about x.rs with a bogus anchor commit.", &ctx);
    let resolver = FileExistsResolver { repo_root: root.to_path_buf() };
    let verdicts = replay(root, &claims, &resolver).unwrap();
    assert_eq!(verdicts.len(), 1);
    assert!(verdicts[0].note.as_deref().unwrap_or("").contains("rev-list failed"));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ocean-context --test replay_git`
Expected: compile error — `replay` not implemented.

- [ ] **Step 3: Implement `src/replay.rs`**

```rust
//! The replay harness: walk a repo's git history forward from each claim's
//! anchor commit, resolving anchors at every commit. The first commit at
//! which no anchor resolves is the verdict a human judges against reality.
//! This is the same code path production reverification uses — you tune the
//! replay; the thing you tuned is the engine.

use crate::claim::Claim;
use crate::seams::{Resolution, Resolver};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

#[derive(Debug)]
pub struct ReplayVerdict {
    pub claim_id: String,
    /// First 60 chars of the claim text, for human-readable verdict tables.
    pub claim_text: String,
    pub anchor_commit: String,
    pub commits_walked: usize,
    /// First commit (full sha) at which no anchor resolved. None = held to HEAD.
    pub first_fail_commit: Option<String>,
    /// Set when the claim couldn't be replayed (no anchors, bad anchor commit).
    pub note: Option<String>,
}

/// Replay `claims` against `repo_root`'s history from each claim's
/// `provenance.commit_sha` (exclusive) to HEAD.
pub fn replay(
    repo_root: &Path,
    claims: &[Claim],
    resolver: &dyn Resolver,
) -> Result<Vec<ReplayVerdict>> {
    let mut verdicts = Vec::new();
    for claim in claims {
        let mut verdict = ReplayVerdict {
            claim_id: claim.id.clone(),
            claim_text: claim.text.chars().take(60).collect(),
            anchor_commit: claim.provenance.commit_sha.clone(),
            commits_walked: 0,
            first_fail_commit: None,
            note: None,
        };
        if claim.provenance.anchors.is_empty() {
            verdict.note = Some("no structural anchors — nothing to replay".to_string());
            verdicts.push(verdict);
            continue;
        }
        let commits = match rev_list(repo_root, &claim.provenance.commit_sha) {
            Ok(c) => c,
            Err(e) => {
                verdict.note = Some(format!("rev-list failed: {e}"));
                verdicts.push(verdict);
                continue;
            }
        };
        verdict.commits_walked = commits.len();
        for commit in &commits {
            let resolves = claim
                .provenance
                .anchors
                .iter()
                .any(|a| matches!(resolver.resolve(a, commit), Resolution::Resolves(_)));
            if !resolves {
                verdict.first_fail_commit = Some(commit.clone());
                break;
            }
        }
        verdicts.push(verdict);
    }
    Ok(verdicts)
}

fn rev_list(repo_root: &Path, from: &str) -> Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-list", "--reverse", &format!("{from}..HEAD")])
        .output()
        .context("running git rev-list")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect())
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p ocean-context --test replay_git`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
but status --json
but commit ocean-context-layer-a -m "feat(ocean-context): replay harness over real git history" --changes <ids>
```

---

### Task 9: The replay binary + a real run on ocean-os

**Files:**
- Modify: `crates/ocean-context/src/bin/replay.rs`

- [ ] **Step 1: Implement the binary** (replace the placeholder):

```rust
//! Replay a prose handoff's claims against a repo's real git history and
//! print per-claim verdicts for a human to judge.
//!
//!   ocean-context-replay --repo ~/dev/ocean-os --doc HANDOFF.md --anchor <sha>

use anyhow::{Context, Result};
use clap::Parser;
use ocean_context::extract::{extract_claims, ExtractCtx};
use ocean_context::replay::replay;
use ocean_context::seams::FileExistsResolver;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Replay handoff claims against real git history")]
struct Args {
    /// Repo whose history to walk
    #[arg(long)]
    repo: PathBuf,
    /// Prose HANDOFF.md to extract anchored claims from
    #[arg(long)]
    doc: PathBuf,
    /// Commit the claims are anchored at (the clock they are dated against)
    #[arg(long)]
    anchor: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let text = std::fs::read_to_string(&args.doc)
        .with_context(|| format!("reading {}", args.doc.display()))?;
    let ctx = ExtractCtx { commit_sha: &args.anchor, now: 0, by_session: "replay-bin" };
    let claims = extract_claims(&text, &ctx);
    eprintln!("extracted {} anchored claims from {}", claims.len(), args.doc.display());

    let resolver = FileExistsResolver { repo_root: args.repo.clone() };
    let verdicts = replay(&args.repo, &claims, &resolver)?;

    let (mut held, mut failed, mut skipped) = (0usize, 0usize, 0usize);
    for v in &verdicts {
        let fate = match (&v.first_fail_commit, &v.note) {
            (_, Some(n)) => {
                skipped += 1;
                format!("SKIP  ({n})")
            }
            (Some(c), _) => {
                failed += 1;
                format!("FAIL @ {}", &c[..10.min(c.len())])
            }
            (None, _) => {
                held += 1;
                format!("HELD  through {} commits", v.commits_walked)
            }
        };
        println!("{:<5} {:<62} {fate}", v.claim_id, v.claim_text);
    }
    eprintln!("\n{held} held, {failed} failed, {skipped} skipped — judge the FAILs against reality.");
    Ok(())
}
```

- [ ] **Step 2: Build and run on the real corpus** (acceptance criterion #4):

```bash
cargo build -p ocean-context --release
ANCHOR=$(git -C /Users/risingtidesdev/dev/ocean-os log --reverse --format=%H -- HANDOFF.md | head -1)
./target/release/ocean-context-replay \
  --repo /Users/risingtidesdev/dev/ocean-os \
  --doc /Users/risingtidesdev/dev/ocean-os/HANDOFF.md \
  --anchor "$ANCHOR"
```

Expected: a verdict table over the 22 ocean-os claims — some HELD, some FAIL at a specific commit. Capture this output verbatim; it goes in the final report for John to judge. (Verdict QUALITY is judged by John, not by this plan — the acceptance criterion is that the harness runs end-to-end and prints first-fail commits.)

- [ ] **Step 3: Commit**

```bash
but status --json
but commit ocean-context-layer-a -m "feat(ocean-context): replay binary — verdict tables over real history" --changes <ids>
```

---

### Task 10: Acceptance sweep — clippy, full tests, handoff update

**Files:**
- Modify: `docs/specs/HANDOFF-ocean-context.md`

- [ ] **Step 1: Full test + clippy pass** (acceptance criterion #5)

```bash
cargo test -p ocean-context
cargo clippy -p ocean-context --all-targets -- -D warnings
```

Expected: all tests green, zero clippy warnings. Fix anything that surfaces.

- [ ] **Step 2: Update the hand-authored handoff** — in `docs/specs/HANDOFF-ocean-context.md`:
  - Claim `c1`: append a history event `{ at: <now>, event: reverified, by_session: <this session> }` and flip `status: Asserted` → `Verified` with a note that the crate now exists at `crates/ocean-context`.
  - Under "Schema/spec friction", append any NEW findings hit during implementation (the handoff explicitly asks for this — it is the beta test of the schema). At minimum record how F1 was resolved (stub stays file-exists + basename fallback; tree-sitter is B1).

- [ ] **Step 3: Final commit**

```bash
but status --json
but commit ocean-context-layer-a -m "docs(ocean-context): close the loop on the hand-authored handoff" --changes <ids>
```

- [ ] **Step 4: Do NOT push or open a PR yet.** John must review the seam signatures (`seams.rs`) first — that's the architectural bet. Surface them in the completion report.

---

## Self-Review Notes

- **Spec coverage:** schema (Task 2), extract + 51-claim regression (Tasks 3–4 → acceptance #1), store round-trip (Task 5 → acceptance #2), read_freshest sorted by stub trust (Task 7 → acceptance #3), four seams verbatim (Task 6), replay binary on real history (Tasks 8–9 → acceptance #4), clippy/test sweep (Task 10 → acceptance #5). Zero LLM calls anywhere.
- **F1** resolved per the handoff's own recommendation; **F3** addressed via `derive_confidence`; **F4** via store-owned TOML frontmatter; **F5** via empty-lines anchors + basename fallback + the file-only anchor test.
- Known v1 limitation (document, don't fix): symbol→anchor pairing is positional zip; symbol-only anchors (no file on the line) are not extracted — that's pass-2/B1 territory.
