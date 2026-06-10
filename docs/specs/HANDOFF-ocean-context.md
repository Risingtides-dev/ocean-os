---
# === ocean-context handoff (hand-authored — BETA TEST of the schema itself) ===
# There is no engine yet. This doc was written BY HAND in the v1 schema so the next agent
# consumes a real codified handoff and reports where the schema chafes. You are the manual
# run of the engine we're about to build. Treat schema friction as a finding, not an annoyance.
session_id: brainstorm-2026-06-09-ocean-context
parent_session: null
repo: ocean-os
branch: fix/ocean-220-livekit-token-auth   # NOTE: we branched the spec onto an unrelated branch; see claim c7
commit_anchor: d9a9bc9
scope_ring: Repo
velocity_at_write: { v_code: 0.0, v_sem: 0.0 }   # v1: zeros. Layer B fills these.
written_at: 1780980000
---

# Handoff — ocean-context (the handoff/context engine itself)

> **For the agent picking this up.** We just spent a long brainstorm designing a context engine
> where the handoff is a *primitive*. The full design is committed at
> `docs/specs/ocean-context-handoff-engine.md`. **Read that spec first — it is the source of truth.**
> This handoff is the codified, claim-level state of *where we are* and *what's next*, and it is
> itself the first beta-test artifact of the schema that spec defines.

## How to pick this up

1. Read `docs/specs/ocean-context-handoff-engine.md` end to end (Layer A is what you build).
2. Read the brainstorm visual artifacts (optional, for intuition) in
   `claude-monorepo/.superpowers/brainstorm/98485-1780979824/content/` — `master-equation.html`
   animates the whole algorithm; `decay-lab.html` is the tunable forgetting curves.
3. The pass-1 claim extractor prototype (Python, throwaway — you'll rewrite in Rust) is at
   `claude-monorepo/.superpowers/brainstorm/sim/extract_claims.py`. It proved the schema on 51 real claims.
4. **Do NOT start with Layer B.** Build Layer A. The whole bet is the trait seams.

## Narrative state (what happened, in prose — humans read this)

John wants a handoff system that "punches above its weight": files/names handoffs sorted by context
freshness, diffs context docs against each other, caches context, applies a weighting + embeddings,
then collapses the session and reinjects context into a fresh one. The brainstorm grounded that
ambition in four papers and arrived at a two-layer plan: a **small handoff doc engine** (Layer A,
build now) whose **trait seams** are filled by **primitives** (Layer B) that grow it into the full
context engine. The crate is named `ocean-context` and lives in the ocean-os workspace. Tuning is by
**replaying real ocean-os git history** against real anchored claims, with a human judging verdicts.

## Claims (the codified, machine-checkable state)

- id: c1
  text: "ocean-context is a NEW crate to be added at crates/ocean-context in the ocean-os workspace; it does not exist yet."
  provenance: { anchors: [{ file: "Cargo.toml", symbol: "workspace.members", lines: [] }], ticket: null, commit_sha: d9a9bc9 }
  status: Asserted        # this is a plan, not yet reproducible — correctly NOT Verified
  knowledge_tier: Individual
  confidence: 1.0
  history: [{ at: 1780980000, event: written, by_session: brainstorm-2026-06-09-ocean-context }]

- id: c2
  text: "The full design (Layer A build + Layer B backlog + master equation + theory provenance) is committed and is the source of truth."
  provenance: { anchors: [{ file: "docs/specs/ocean-context-handoff-engine.md", symbol: null, lines: [] }], ticket: null, commit_sha: d9a9bc9 }
  status: Verified        # this file exists right now; reverify by confirming it's on disk
  knowledge_tier: Common
  confidence: 1.0
  history: [{ at: 1780980000, event: written, by_session: brainstorm-2026-06-09-ocean-context }]

- id: c3
  text: "The architectural bet is the four trait seams (Resolver, TrustModel, Retriever, Borrower); Layer A declares them with stub impls, Layer B fills them. Scrutinize these signatures in review."
  provenance: { anchors: [{ file: "docs/specs/ocean-context-handoff-engine.md", symbol: "trait Resolver", lines: [] }], ticket: null, commit_sha: d9a9bc9 }
  status: Verified
  knowledge_tier: Common
  confidence: 0.9         # <1.0: seam signatures are a hypothesis, may need revision once code is real
  history: [{ at: 1780980000, event: written, by_session: brainstorm-2026-06-09-ocean-context }]

- id: c4
  text: "Schema validated against reality: regex anchor extraction pulled 51 real anchored claims from 2 root HANDOFF.md docs."
  provenance: { anchors: [{ file: "claude-monorepo/.superpowers/brainstorm/sim/extract_claims.py", symbol: "extract", lines: [] }, { file: "HANDOFF.md", symbol: null, lines: [] }], ticket: null, commit_sha: d9a9bc9 }
  status: Verified
  knowledge_tier: Individual   # verified by THIS session's run; another agent should re-run to confirm
  confidence: 0.85
  history: [{ at: 1780980000, event: written, by_session: brainstorm-2026-06-09-ocean-context }]

- id: c5
  text: "Tuning method is REPLAY of real ocean-os history (218 commits) + the 51 real claims; a human judges REVERIFY verdicts. NOT a synthetic oracle. Decided by John."
  provenance: { anchors: [{ file: "docs/specs/ocean-context-handoff-engine.md", symbol: "Proof / tuning method", lines: [] }], ticket: null, commit_sha: d9a9bc9 }
  status: Verified
  knowledge_tier: Common
  confidence: 1.0
  history: [{ at: 1780980000, event: written, by_session: brainstorm-2026-06-09-ocean-context }]

- id: c6
  text: "Resolver depth for v1 replay is FULL tree-sitter AST resolution (symbol-as-function + signature hash → Stale/Renamed/Dead), not mere file-exists. Decided by John."
  provenance: { anchors: [{ file: "docs/specs/ocean-context-handoff-engine.md", symbol: "Resolution", lines: [] }], ticket: null, commit_sha: d9a9bc9 }
  status: Verified
  knowledge_tier: Common
  confidence: 0.9         # NOTE the tension: spec's Layer-A stub Resolver is file-exists; John chose tree-sitter as the v1 replay oracle. See "Schema/spec friction" below — RESOLVE THIS.
  history: [{ at: 1780980000, event: written, by_session: brainstorm-2026-06-09-ocean-context }]

- id: c7
  text: "The 80 worktree HANDOFF.md files are byte-identical (one md5) — NOT 80 distinct handoffs. The real distinct corpus is 2 root docs. Do not treat worktree handoffs as data."
  provenance: { anchors: [{ file: ".claude/worktrees", symbol: null, lines: [] }], ticket: null, commit_sha: d9a9bc9 }
  status: Verified
  knowledge_tier: Individual
  confidence: 0.9
  history: [{ at: 1780980000, event: written, by_session: brainstorm-2026-06-09-ocean-context }]

- id: c8
  text: "Neo4j is an EARNED upgrade for B6 (epistemic similarity graph / multi-hop edges) only. pgvector already exists in the brain — reuse it for B3 retrieval, do not rebuild. Do not pull either into Layer A."
  provenance: { anchors: [{ file: "docs/specs/ocean-context-handoff-engine.md", symbol: "Staging discipline", lines: [] }], ticket: null, commit_sha: d9a9bc9 }
  status: Verified
  knowledge_tier: Common
  confidence: 1.0
  history: [{ at: 1780980000, event: written, by_session: brainstorm-2026-06-09-ocean-context }]

## Next concrete steps (in order)

1. Run brainstorming's writing-plans skill against the spec to produce the Layer A implementation plan.
2. Scaffold `crates/ocean-context` (lib-first), add to `Cargo.toml` workspace members.
3. Build `claim.rs` (the serde schema) + `extract.rs` (port the Python regex pass, regression-test on the 51 claims).
4. Declare the four traits in `seams.rs` with trivial stub impls — get John to review the SIGNATURES before going further.
5. Build `replay.rs` + the replay binary; walk ocean-os history, print verdicts John judges.

## Schema/spec friction found while hand-authoring this (THE BETA-TEST FINDINGS)

These are the whole point of writing this by hand. Surface more as you consume it.

- **F1 — Resolver depth contradiction (see c6).** The spec's Layer-A `Resolver` stub is "file-exists,"
  but John chose **tree-sitter** as the v1 replay oracle. Reconcile: either v1's stub stays file-exists
  and tree-sitter is "B1 built immediately as part of A's acceptance," or the spec's Layer-A scope
  expands to include tree-sitter. **Recommend the former** (keep the seam stub trivial; make B1 the
  first thing built against it) so the trait boundary stays honest. RESOLVE before planning.
- **F2 — `status` vs `knowledge_tier` overlap.** Writing claims by hand, the two enums felt redundant
  at times (a Common-tier claim is ~always Verified). Consider whether `knowledge_tier` is derivable
  from `status` + ring-spread, or genuinely orthogonal. Lean: orthogonal (tier = WHERE it's knowable,
  status = whether it currently reproduces), but the engine should compute tier, not ask the writer to.
- **F3 — confidence is hand-wavy when written by a human.** I assigned 0.85/0.9/1.0 by feel. v1 should
  probably DERIVE write-time confidence from anchor count + declared_verified, not free-type it.
- **F4 — YAML-in-frontmatter + claims-as-prose-list is awkward to hand-write and will be awkward to
  parse robustly.** The codified claims want to be a structured block (the engine writes them), while
  the narrative stays markdown. v1 `store.rs` should own this format so humans never hand-edit claims.
- **F5 — anchors with empty `lines: []` are common** (many real claims reference a file/symbol with no
  line). The Resolver must handle symbol-only and file-only anchors gracefully, not assume line numbers.
