# Code Context

## Files Retrieved
1. `AGENTS.md` (entire file) — repository ownership, core flow, daemon/session/cwd invariants, and workspace gates.
2. `crates/AGENTS.md` (entire file) — canonical package ownership, entry points, exclusions, validations, cross-crate fanout, and non-default members.
3. `crates/ocean-agent/AGENTS.md` (entire file) — session rebind contract.
4. `crates/ocean-protocol/AGENTS.md` (entire file) — provider wire invariants.
5. `crates/ocean-runtime/AGENTS.md` (entire file) — permission, cwd, and tool execution contracts.
6. `crates/ocean-tui/AGENTS.md` (entire file) — shared `Action` and Elm-loop contracts.
7. `crates/ocean-daemon/AGENTS.md` (entire file) — HTTP/SSE and caller-cwd contracts.

No source files were required. No case required deep source search. The prohibited archive and plan were not read.

## Key Code

### 1. Resumed session loses history after workspace rebind
- **Owner repo/crate:** `risingtides-dev/ocean-os` / `ocean-agent` (coordinate `ocean-daemon`).
- **Primary entry file/symbol:** `crates/ocean-agent/src/lib.rs` — session load/save and bind path.
- **One critical invariant:** Every bind refreshes `cwd`; `workspace_root` and git metadata change when the caller moves workspaces, while persisted transcript compatibility/history is preserved.
- **Narrow validation command:** `cargo test -p ocean-agent`.

### 2. Anthropic/OpenAI/Gemini/Codex wire encoding
- **Owner repo/crate:** `risingtides-dev/ocean-os` / `ocean-protocol`.
- **Primary entry file/symbol:** `crates/ocean-protocol/src/lib.rs`, `crates/ocean-protocol/src/providers/` — provider-specific codecs.
- **One critical invariant:** Provider quirks remain isolated behind protocol abstractions; streaming event shape is compatibility-sensitive.
- **Narrow validation command:** `cargo test -p ocean-protocol`.

### 3. Filesystem/bash tool wrong cwd or permission bypass
- **Owner repo/crate:** `risingtides-dev/ocean-os` / `ocean-runtime` (daemon/agent fanout as needed).
- **Primary entry file/symbol:** `crates/ocean-runtime/src/lib.rs`, `crates/ocean-runtime/src/agent_loop.rs` — tool dispatch/execution.
- **One critical invariant:** Filesystem/process tools resolve against the turn's `SessionContext.cwd`, never daemon process cwd, and every execution path passes mandatory permission gates.
- **Narrow validation command:** `cargo test -p ocean-runtime`.

### 4. TUI keybinding/shared `Action` variant
- **Owner repo/crate:** `risingtides-dev/ocean-os` / `ocean-tui`.
- **Primary entry file/symbol:** `crates/ocean-tui/src/shell/action.rs::Action`.
- **One critical invariant:** `Action` variants are additive; components emit actions and mutation occurs only through `App::dispatch`/component `update`.
- **Narrow validation command:** `cargo test -p ocean-tui && cargo build -p ocean-tui --release` (add `cargo check --workspace --tests` when a shared enum changes).

### 5. Daemon HTTP/SSE route with caller cwd semantics
- **Owner repo/crate:** `risingtides-dev/ocean-os` / `ocean-daemon`.
- **Primary entry file/symbol:** `crates/ocean-daemon/src/main.rs` — HTTP/SSE router and turn orchestration.
- **One critical invariant:** Effective cwd comes from client cwd/project metadata and never falls back to daemon process cwd or a resumed session's first cwd.
- **Narrow validation command:** `cargo test -p ocean-daemon`.

### 6. PSTN/LiveKit call pipeline
- **Owner repo/crate:** `risingtides-dev/ocean-os` / `ocean-call`.
- **Primary entry file/symbol:** `crates/ocean-call/src/lib.rs`, `crates/ocean-call/src/session_task.rs`.
- **One critical invariant:** PSTN/Twilio/LiveKit audio and call intelligence belong in `ocean-call`; daemon HTTP route ownership does not.
- **Narrow validation command:** `cargo test -p ocean-call`.

### 7. Longhouse quorum/title/revocation behavior
- **Owner repo/crate:** `risingtides-dev/ocean-os` / `ocean-longhouse`.
- **Primary entry file/symbol:** `crates/ocean-longhouse/src/lib.rs`, `quorum.rs`, `escrow.rs`.
- **One critical invariant:** Quorum, titles, escrow, recall/revocation, and preparation remain Longhouse-owned; daemon permission/execution authority remains outside it.
- **Narrow validation command:** `cargo test -p ocean-longhouse`.

### 8. External MCP tool versus subprocess plugin tool
- **Owner repo/crate:** `risingtides-dev/ocean-os` / external MCP: `ocean-mcp`; subprocess plugin: `ocean-plugin`.
- **Primary entry file/symbol:** MCP: `crates/ocean-mcp/src/lib.rs`, `provider.rs`; plugin: `crates/ocean-plugin/src/lib.rs`, `plugin.rs`.
- **One critical invariant:** External MCP connections/adapters must not absorb subprocess-plugin transport/lifecycle, and subprocess plugins must not implement external MCP transport.
- **Narrow validation command:** `cargo test -p ocean-mcp` or `cargo test -p ocean-plugin`, according to the selected mechanism.

### 9. Handoff extraction versus hashline edits versus AST summarization
- **Owner repo/crate:** `risingtides-dev/ocean-os` / extraction: `ocean-context`; edits: `ocean-hashline`; AST summaries: `ocean-ast`.
- **Primary entry file/symbol:** `crates/ocean-context/src/extract.rs`; `crates/ocean-hashline/src/patcher.rs`; `crates/ocean-ast/src/lib.rs::summarize_code`.
- **One critical invariant:** Evidence-bearing handoff extraction, file-hash-anchored mutation, and read-time structural summarization remain separate; `ocean-ast` is standalone and not live-runtime wired.
- **Narrow validation command:** Respectively `cargo test -p ocean-context`, `cargo test -p ocean-hashline`, or `cargo test -p ocean-ast`.

### 10. Add/validate workspace package including non-default members
- **Owner repo/crate:** `risingtides-dev/ocean-os` / workspace root plus `crates/AGENTS.md` canonical index (`xtask` is root-level; `ocean-ast` is non-default).
- **Primary entry file/symbol:** `Cargo.toml` `[workspace]` members/default-members and `crates/AGENTS.md` Workspace Package Index.
- **One critical invariant:** Package membership and the canonical index stay in parity; non-default packages such as `ocean-ast` must be validated explicitly or with `--workspace`.
- **Narrow validation command:** `cargo metadata --no-deps --format-version=1` (then explicit `cargo test -p <package>` for a non-default package).

## Architecture

Routing follows client → `ocean-daemon` → `ocean-agent` → `ocean-runtime` → `ocean-protocol`/`ocean-providers`. Specialized packages own calls, Longhouse, MCP/plugins, and three distinct code-context operations. `crates/AGENTS.md` is the canonical package routing/index authority.

## Start Here

Open `crates/AGENTS.md` first after root `AGENTS.md`; it answers ownership, entry point, exclusions, and narrow validation for all ten cases and links the five local contracts needed for sharper invariants.

## Benchmark Summary

- **Total cases answered:** 10/10.
- **Deep source searches required:** 0/10.
- **Ambiguity:** Case 8 is intentionally conditional on tool transport (external MCP vs subprocess); case 9 is intentionally conditional on operation type. Case 1's index identifies `lib.rs` but not a named bind symbol, so the entry is file-level without source search. Case 10's first validation checks index parity; package logic still requires that package's explicit test.

```acceptance-report
{
  "criteriaSatisfied": [
    {
      "id": "criterion-1",
      "status": "satisfied",
      "evidence": "Produced the requested 10-case cold-agent routing benchmark at the authoritative /tmp path without modifying repository files or reading prohibited paths."
    },
    {
      "id": "criterion-2",
      "status": "satisfied",
      "evidence": "Each case includes owner, primary entry, one invariant, and narrow validation; retrieved files and deep-search usage are explicitly reported."
    }
  ],
  "changedFiles": [
    "/tmp/ocean-agent-readiness-post-1.md"
  ],
  "testsAddedOrUpdated": [],
  "commandsRun": [
    {
      "command": "Read AGENTS.md, crates/AGENTS.md, and five linked crate AGENTS.md contracts",
      "result": "passed",
      "summary": "All 10 cases were routable without source search."
    },
    {
      "command": "git status --porcelain && git diff --cached --name-only",
      "result": "passed",
      "summary": "Repository had pre-existing unstaged/untracked files; cached diff output was empty."
    }
  ],
  "validationOutput": [
    "10/10 cases answered; 0/10 required deep source search.",
    "No staged files detected by git diff --cached --name-only.",
    "No repository files modified by this task."
  ],
  "residualRisks": [
    "File-level entry points are reported where the canonical routing docs do not name a narrower symbol; confirming narrower symbols would violate the benchmark's source-search constraint because the docs were sufficient."
  ],
  "noStagedFiles": true,
  "diffSummary": "No repository diff created; one requested report written under /tmp.",
  "reviewFindings": [
    "no blockers"
  ],
  "manualNotes": "The working tree was already dirty with unrelated unstaged/untracked files; none were touched. Prohibited docs/.agentarchive content and the named plan were not read."
}
```
