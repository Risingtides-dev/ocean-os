# Ocean OS Cold-Agent Routing Baseline

**Date:** 2026-07-12
**Repository state:** `main` at `b5d564169f3f7034c2794007ed9795be3e6bb498`
**Purpose:** Pre-Phase-0A benchmark for the code-health and agent-readiness plan.

## Method

Three `scout` agents ran concurrently with fresh context and low reasoning effort. They started at the repository root, could inspect active repository files, and were prohibited from reading `docs/.agentarchive/` or the active readiness plan.

Each agent received the same ten cases and had to return:

1. owner repo/crate;
2. primary entry file/symbol;
3. one critical invariant;
4. narrow validation command.

The parallel fleet completed in approximately 101 seconds. Raw outputs are retained as opt-in historical evidence under `docs/.agentarchive/`; they are not required onboarding context. SHA-256:

- run 1: `791813bc7474d7e34741fccd187fef2a7f3c760a04d22e6f39ed34f8916e638d`
- run 2: `528b13ee27b96db6348367f536f3105c4934e4d44846b88dbb651dc2624ae96c`
- run 3: `06dc64ac68ac25bc154605643ae369aa65b89599ff013ba5dffbd62450dd2ad4`

## Fixed corpus

1. Resumed session loses history after workspace rebind.
2. Add/fix Anthropic, OpenAI, Gemini, or Codex wire encoding.
3. Filesystem/bash tool uses the wrong cwd or bypasses permission.
4. Add a TUI keybinding/shared `Action` variant.
5. Add/change a daemon HTTP/SSE route with caller-cwd semantics.
6. Change the PSTN/LiveKit call pipeline.
7. Change Longhouse quorum/title/revocation behavior.
8. Add an external MCP tool versus subprocess plugin tool.
9. Change handoff extraction versus hashline edits versus AST summarization.
10. Add/validate a workspace package including non-default members.

## Results

| Run | Correct routing | Time bound | Main miss/ambiguity |
|---|---:|---:|---|
| 1 | 9/10 | <101s | Routed the TUI case to legacy `main.rs::Action` instead of the active `shell/action.rs::Action`. |
| 2 | 9/10 | <101s | Same active-vs-legacy TUI `Action` miss. |
| 3 | 10/10 | <101s | Correctly used `shell/action.rs`; cross-owner cases remained explicitly split. |
| **Total** | **28/30** | **under five minutes** | TUI dual-surface ambiguity is the only repeated routing error. |

## Findings

- Search-capable agents already route most work correctly by reading source and the eight existing local contracts.
- The incomplete crate indexes did not cause broad wrong answers, but agents had to rediscover ownership through grep/source traversal.
- The retained legacy TUI creates a concrete discoverability failure: two of three agents selected the wrong `Action` enum.
- Cross-boundary requests need explicit owner/exclusion language:
  - session persistence (`ocean-agent`) versus daemon route binding;
  - call domain (`ocean-call`) versus daemon HTTP integration;
  - MCP versus plugin as separate extension paths;
  - context extraction, hashline mutation, and AST summarization as separate crates.
- `ocean-ast` and `xtask` were identified as non-default members, but the repository did not state why `ocean-ast` was excluded.
- Several suggested test filters were plausible but unverified; canonical docs should prefer stable crate-wide commands unless a local contract names a maintained filter.

## Post-Phase-0A result

The same corpus was repeated after installing the canonical 25-package index and root routing contract. Three fresh agents completed concurrently in approximately 78 seconds.

| Run | Correct routing | Deep source search | Result |
|---|---:|---:|---|
| 1 | 10/10 | 0/10 | Active `shell/action.rs::Action` selected correctly. |
| 2 | 10/10 | 4/10 | Docs identified every owner/file; source search refined symbols for session/runtime/daemon/call cases. |
| 3 | 10/10 | 0/10 | All cases answered from root/index/local contracts/project map. |
| **Total** | **30/30** | **4/30** | No repeated TUI ambiguity; non-default rationale stated directly. |

Compared with baseline:

- routing accuracy improved from **28/30 to 30/30**;
- the repeated active-vs-legacy TUI error fell from 2/3 runs to 0/3;
- two agents needed no source search at all; the third used source only to name narrower symbols;
- fleet time improved from approximately 101 seconds to 78 seconds (about 23% faster, directional only).

Post-change raw outputs are retained as opt-in historical evidence under `docs/.agentarchive/`. SHA-256:

- run 1: `3b09e794916e4ceafa1260972c12e7f59817a0f54e88180023202b377b7da3fc`
- run 2: `a86c1bd88bde63ab1cee539b5f0190c2454df15cadca55066ab3d4045daaa2d8`
- run 3: `4d43a2a2bb996c4ee18f384624dd744ea4e740f6658eaacf59d46c4ef40a4348`

**Phase 0A navigation acceptance: PASS.** Precise symbol discovery can still require source inspection, but owner, boundary, entry file, invariant, and stable narrow validation are now available from active routing contracts.
