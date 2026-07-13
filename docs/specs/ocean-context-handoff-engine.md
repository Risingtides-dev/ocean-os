---
spec: ocean-context
status: layer-a-shipped   # Layer A merged 2026-06-10 (PR #203, OCEAN-306); Layer B is backlog
date: 2026-06-09
updated: 2026-06-10
author: john + Claude (brainstorm session)
crate: crates/ocean-context
---

# ocean-context — the handoff as a context primitive

> **One line:** A handoff is not a trusted prose blob. It is a set of **claims**, each with
> provenance, that the receiving session **distrusts by default** and **reverifies against ground
> truth** before acting on. The context engine is the machinery that scores, reverifies, and
> reconstructs that trust.

This spec is deliberately in **two layers**. Build Layer A. Layer B is the named backlog of
primitives that grow A into the full engine *without changing A's shape*. The whole architectural
bet is the **trait seams** between them: get those right and the context engine is just the doc
engine with better organs.

> **Status (2026-06-10): Layer A is SHIPPED** — `crates/ocean-context` on main via PR #203
> (OCEAN-306). All five acceptance criteria below are met: the 51-claim corpus is
> regression-locked (22+29), store round-trips losslessly, `read_freshest` sorts by the stub
> TrustModel, the replay binary walked real ocean-os history, tests + clippy clean, zero LLM calls.
> First real replay (22 claims × ~180 commits): 21 HELD, 1 FAIL — the FAIL being a *cross-repo*
> anchor, which is correct-but-uninformative (out-of-ring, not Dead). Build findings F6–F8 live in
> `HANDOFF-ocean-context.md`; F7 (file-exists is too forgiving an oracle to produce judgeable
> verdicts) makes **B1 the confirmed next step**.

---

## Why (the problem)

Context rot across session boundaries. A handoff says "auth uses JWT, validated in `verifyToken()`
at `auth.ts:42`". Three sessions later the file moved, the symbol changed signature, the decision
was reversed — but the next agent swallows the stale claim whole and builds on a lie. Today's
`HANDOFF.md` flow is good prose and completely untrusted-by-machine. We make every claim
**machine-checkable** and **falsifiable**.

This came out of a brainstorm grounded in four papers (see `## Theory provenance`). The headline
behaviors:

1. **Codified claims** — handoffs carry structured, anchored claims, not just prose.
2. **Distrust-by-default + reverification** — a claim is only trusted if its anchor still resolves.
3. **Concentric scope rings** — session ▸ branch ▸ repo ▸ brain, weighted down as they widen.
4. **Velocity-modulated decay** — trust half-life adapts to project churn. Flexible early
   (molten, everything provisional, reverify cheap), strict in prod (frozen, a broken anchor is a
   real alarm). **The system tapers itself** — it measures whether the codebase's representational
   geometry has crystallized, and stiffens trust accordingly.
5. **Feature-borrowing** — a thin fresh claim can *borrow* trust from a richer, higher-PS ancestor
   in the same concept-subspace (distributed knowledge), so weak signals punch above their weight.

---

## The master equation (the north star, Layer B realizes it)

```
T(c, t) = RRF(bm25, emb) · w_ring · S(c)·PS(anchor) · e^(−λ(v)·Δt)

  RRF(bm25,emb)  relevance to the receiving task   — Lucene/BM25 ⊕ embedding, reciprocal-rank-fused
  w_ring         concentric scope weight           — git proximity × subspace alignment
  S(c)           structural reproducibility ∈[0,1] — does the AST anchor still resolve? (the oracle)
  PS(anchor)     parallelism score of the concept  — Wang-Fusi: is the anchor a clean/abstract axis?
  e^(−λ(v)·Δt)   velocity-modulated forgetting      — λ(v)=ln2/H, H = H₀·e^(−κ·v_sem)
  v_sem          rate of disentanglement           — d/dt of the representation-kernel geometry

claim status (epistemic tier — model-checked over the similarity graph):
  Common knowledge       → VERIFIED, no reverify   (reproduces across ALL rings/worlds)
  Individual knowledge   → VERIFIED for this session (reproduces with this agent's ability)
  Distributed knowledge  → BORROW                   (not verifiable alone; group/ancestor can)
  none                   → REVERIFY / DEAD          (no agent or group reproduces → ground-truth check)
```

**v1 does NOT compute this.** v1 ships the *struct that holds these fields* and a stub `TrustModel`
that returns a trivial score. Layer B fills each term in behind the trait. The equation is the
contract, not the day-one implementation.

---

## Layer A — the Handoff Doc Engine (SHIPPED — PR #203)

Small, real, useful day one. Replaces hand-written `HANDOFF.md` with typed, anchored claim-sets.
**No tree-sitter, no embeddings, no graph.** Just: write a handoff, read the freshest, with
provenance. Library-first — the replay binary and the eventual daemon both consume `ocean_context`
the lib.

### Schema (serde structs — the codified handoff)

```rust
/// A handoff = metadata + an ordered set of claims. Human-readable prose stays in `narrative`;
/// the machine-checkable substance is `claims`.
struct Handoff {
    session_id: String,
    parent_session: Option<String>,   // similarity-graph edge
    repo: String,
    branch: String,
    commit_anchor: String,            // the clock claims are dated against (short sha)
    scope_ring: ScopeRing,            // Session | Branch | Repo | Brain
    velocity_at_write: Velocity,      // {v_code, v_sem} snapshot (v1: zeros, Layer B fills)
    written_at: i64,                  // unix seconds (passed in — no Date::now in pure cores)
    narrative: String,                // the prose handoff, unchanged; humans still read this
    claims: Vec<Claim>,
}

struct Claim {
    id: String,
    text: String,                     // "reads ungated, mutators implement requires_permission()"
    provenance: Provenance,
    status: ClaimStatus,              // Verified | Reverify | Stale | Dead | Asserted
    knowledge_tier: KnowledgeTier,    // Common | Individual | Distributed (v1: defaults to Individual)
    ps_anchor: Option<f32>,           // Layer B (Wang-Fusi). None in v1.
    confidence: f32,                  // trust at write-time
    borrowed_from: Option<String>,    // distributed-knowledge edge (Layer B)
    history: Vec<ClaimEvent>,         // written | reverified | promoted | killed  (self-versioning)
}

struct Provenance {
    anchors: Vec<Anchor>,             // what reverification re-resolves
    ticket: Option<String>,           // OCEAN-16
    commit_sha: String,
}

struct Anchor {
    file: String,                     // crates/ocean-runtime/src/tools/browser/input.rs
    symbol: Option<String>,           // requires_permission
    lines: Vec<u32>,                  // [29, 67, 97, 130]
    sig_hash: Option<String>,         // Layer B (tree-sitter signature hash). None in v1.
}

enum ClaimStatus { Verified, Reverify, Stale, Dead, Asserted }
enum KnowledgeTier { Common, Individual, Distributed }
enum ScopeRing { Session, Branch, Repo, Brain }
struct Velocity { v_code: f32, v_sem: f32 }
struct ClaimEvent { at: i64, event: String, by_session: String }
```

### The trait seams (THE BET — Layer A declares, Layer B implements)

Layer A calls these from day one. Day-one impls are dumb stubs. Layer B swaps real organs in
behind the *exact same traits* without Layer A noticing. If these signatures are wrong, everything
downstream hurts — so this is what the spec review must scrutinize.

```rust
/// Does this claim's anchor still resolve? v1 stub: file-exists. Layer B: tree-sitter AST + sig hash.
trait Resolver {
    fn resolve(&self, anchor: &Anchor, at_commit: &str) -> Resolution;
}
enum Resolution { Resolves(f32), Stale, Renamed, Dead }  // f32 = S(c) ∈ [0,1]

/// Score a claim's live trust. v1 stub: confidence · recency. Layer B: the full master equation.
trait TrustModel {
    fn trust(&self, claim: &Claim, ctx: &TrustContext) -> f32;
}

/// Rank stored claims by relevance to a query. v1 stub: substring match. Layer B: RRF(bm25, emb).
trait Retriever {
    fn rank(&self, query: &str, claims: &[Claim]) -> Vec<(usize, f32)>;
}

/// Can a thin claim borrow trust from a richer ancestor? v1 stub: None. Layer B: distributed knowledge.
trait Borrower {
    fn borrow(&self, claim: &Claim, candidates: &[Claim]) -> Option<Borrowed>;
}
```

### Layer A modules

```
crates/ocean-context/
  src/
    lib.rs        — re-exports, the public API: write_handoff, read_freshest, reverify
    claim.rs      — the serde schema above
    extract.rs    — parse HANDOFF.md prose → Vec<Claim>  (the regex anchor pass, validated on 51 real claims)
    store.rs      — write/read codified handoffs (v1: markdown+frontmatter on disk; Layer B: pg/graph)
    seams.rs      — the four traits + their trivial v1 stub impls
    replay.rs     — the simulation harness: walk git, run Resolver at each commit, emit verdicts
  src/bin/
    replay.rs     — run the replay over a repo, print verdicts for a human to judge
```

### Layer A acceptance (when v1 is done)

> Historical corpus note (2026-07-12): `ocean-os/HANDOFF.md` is now an evergreen pointer, not the old snapshot. The exact validation corpus remains in `crates/ocean-context/tests/fixtures/ocean-os-HANDOFF.md` and opt-in archive history; acceptance must use the fixture, not current root handoff prose.

1. `extract.rs` pulls the 51 known anchored claims out of the two preserved historical HANDOFF fixtures
   (`crates/ocean-context/tests/fixtures/ocean-os-HANDOFF.md` and the corresponding Claude-monorepo snapshot) — deterministic, regression-tested.
2. `write_handoff` round-trips a `Handoff` → markdown+frontmatter → `Handoff` losslessly.
3. `read_freshest(repo, branch)` returns the most recent handoff, claims sorted by the stub TrustModel.
4. The replay binary walks ocean-os history from a claim's anchor commit and prints, per claim, the
   commit at which the stub Resolver first fails — a human judges whether that matches reality.
5. `cargo test` green, `cargo clippy` clean. Zero LLM calls in Layer A.

---

## Layer B — the primitives that grow it into the context engine (backlog)

Each is an independent follow-on that implements a trait Layer A already declared. Build in this
order; each is shippable alone and earns its complexity.

| # | Primitive | Fills | Paper / basis | Earns its place when |
|---|-----------|-------|---------------|----------------------|
| B1 | **Tree-sitter resolver** ← NEXT (confirmed by F7: file-exists held 21/22 over 180 commits — no signal) | `Resolver` | tree-sitter `rust`/`typescript` grammars | symbol-presence + sig-hash beats file-exists on the replay |
| B2 | **Velocity + decay** | `TrustModel` (the e^−λΔt term) | self-tapering half-life | the auto-taper fires at the right maturity on real history |
| B3 | **BM25 + embedding retrieval** | `Retriever` | Lucene → BM25, RRF; pgvector (brain) | hybrid retrieval ranks the right ancestor first |
| B4 | **Parallelism score** | the `PS` term + concept labels | Wang-Fusi (similarity graphs / abstract reps) | PS gates borrowing safely (entangled concepts throttled) |
| B5 | **Feature-borrowing** | `Borrower` | "Learning to Borrow Features" → distributed knowledge | thin claims gain trust from high-PS ancestors w/o hallucinating |
| B6 | **Epistemic similarity graph** | claim-status model-checking, scope edges | "Epistemic Logic over Similarity Graphs" | multi-hop "who-knows-what-through-whom" — **this is where Neo4j earns its place**, not before |
| B7 | **Drift-detection hooks** | the trigger layer | PreCompact/Stop/SessionStart hooks | handoffs auto-write on drift, auto-inject on session start |
| B8 | **Session-lifecycle / relaunch** | collapse + reinject | the novel primitive ocean-os owns | a session can collapse and reconstruct a fresh one in-terminal |

**Staging discipline (non-negotiable):** do NOT pull in Neo4j, embeddings, or the epistemic
model-checker until the trait seam that needs it is proven against the replay. pgvector already
exists in the brain — reuse it (B3), don't rebuild. Neo4j is an *earned* upgrade for B6's typed
multi-hop edges, never an upfront tax.

---

## Proof / tuning method (decided in brainstorm)

**Replay real history, not synthetic.** Tune against ocean-os's 218 real commits + the 51 real
anchored claims. The replay walks the git timeline forward from each claim's anchor commit, runs the
`Resolver` at each step, and emits a timestamped REVERIFY verdict. **A human judges** whether the
flag fired near when the code actually moved (e.g. did the daemon-save-race claim flip around when
the real race regression landed?). Real history surprises you in ways a synthetic oracle can't.

The replay harness IS Layer A's `replay.rs` — the simulation and the production reverification core
are the same code. You tune the replay; the thing you tuned is the engine.

---

## A real-workflow case the design must handle (found during brainstorm)

Fanning a handoff into N parallel agent worktrees produces N byte-identical context copies that
immediately diverge as each agent commits — and nothing tracks the divergence. (Historical observation:
80 worktree HANDOFF.md files shared one md5; the snapshot now lives in the regression fixture/archive.)
This is exactly the epistemic similarity graph: N worlds
that start indistinguishable (`E(s,t) = all abilities`) and split as commits land. B6 must model this
as a first-class case, not a quirk.

---

## Theory provenance (so the next agent can go deep without re-deriving)

- **Lucene `Similarity` (3.6.2)** — `score = coord·queryNorm·Σ(tf·idf²·boost·norm)`. We steal the
  *structure* (coord→multi-anchor confidence, idf→anchor saliency, sublinear tf, length-norm→claim
  specificity) but use **BM25's tuned terms** in the `Retriever` (B3).
- **IBM FileNet CBR ranking** — pure relevance (TF-IDF/BM25). Confirms relevance is only ONE axis;
  we wrap it in the trust field.
- **Wang, Johnston, Fusi — "A mathematical theory for understanding when abstract representations
  emerge"** — Parallelism Score `PS = cos(Δr(k;α₁), Δr(k;α₂))` over the representation kernel K.
  Grounds `PS(anchor)` (B4) and reframes semantic velocity as the *rate of disentanglement*. High PS
  ⇒ clean anchors that reproduce and *generalize across rings* ⇒ how far a handoff's trust can travel.
- **"Learning to Borrow Features for Improved Detection of Small Objects"** — Match→Represent→Fuse.
  Grounds `Borrower` (B5): a thin claim ("small object") borrows from a richer same-class ancestor.
  Guard: only borrow across well-disentangled (high-PS) subspaces, else you hallucinate confidence —
  so the taper protects the borrowing too.
- **"Epistemic Logic over Similarity Graphs — Common, Distributed and Mutual Knowledge"** — the
  unifying frame. Similarity graph `(W, A, E)`: W=version-worlds (sessions/handoffs), A=epistemic
  abilities (an agent's tools/parse-depth), `E(s,t)`=abilities under which two worlds are
  indistinguishable. Gives the 4-tier claim-status type system (Common/Individual/Distributed/none),
  makes reverification a **model-checking query** with known complexity, and justifies the graph DB
  (B6). The paper's own running example is literally collaborators producing divergent doc versions.

---

## Open decisions (for John, deferred — do not block Layer A)

1. Where do codified handoffs physically store in Layer B — pg table, the graph, or stay on-disk?
2. Drift-detection trigger (B7): PreCompact hook vs. periodic vs. Stop hook — pick when B7 starts.
3. Session-relaunch mechanism (B8): in-place `/clear`+inject vs. exec a new `claude` — the riskiest
   primitive, spec it separately when we get there.
4. *(added 2026-06-10, from F6)* Cross-repo anchors: should scope rings reach the `Resolver`
   (a ring-aware `resolve`) so out-of-ring anchors report "foreign", or do replay verdicts just
   carry a foreign-anchor hint until B6? Decide at B1, since the tree-sitter resolver will
   otherwise inherit the same correct-but-wrong "Dead" verdict for them.
