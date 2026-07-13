# Code Context

## Files Retrieved
1. `AGENTS.md` (lines 1-70) - repository routing, global invariants, and completion gates.
2. `crates/AGENTS.md` (lines 1-91) - canonical package ownership, entry points, exclusions, and narrow validation.
3. `crates/ocean-agent/AGENTS.md` (lines 1-32) - session rebind/history contract.
4. `crates/ocean-protocol/AGENTS.md` (lines 1-39) - provider wire invariants.
5. `crates/ocean-runtime/AGENTS.md` (lines 1-50) - cwd, permissions, and tool execution invariants.
6. `crates/ocean-tui/AGENTS.md` (lines 1-75) - `Action`/Elm-loop rules and TUI validation.
7. `crates/ocean-daemon/AGENTS.md` (lines 1-35) - route, SSE, and caller-cwd authority.
8. `docs/OCEAN_PROJECT_MAP.md` (lines 1-132) - cross-repository ownership, especially LiveKit and Longhouse boundaries.
9. `crates/ocean-agent/src/lib.rs` (grep hits around lines 1033-1124 only) - located workspace binding path; deep source search required.
10. `crates/ocean-daemon/src/main.rs` (grep hits around lines 1263 and 1577-1649 only) - located Axum router; deep source search required.
11. `crates/ocean-runtime/src/capability.rs` (grep hits around lines 40-90) and `agent_loop.rs` (389-458) - located session context and permission dispatch; deep source search required.
12. `crates/ocean-call/src/session_task.rs` (grep hits around lines 22-56 and 1186-1397) - located call-loop and LiveKit adapters; deep source search required.

The prohibited `.agentarchive` and readiness-plan files were not read.

## Key Code

### 1. Resumed session loses history after workspace rebind
- **Owner repo/crate:** `risingtides-dev/ocean-os` / `ocean-agent` (coordinate `ocean-daemon`).
- **Primary entry file/symbol:** `crates/ocean-agent/src/lib.rs` — session creation/binding path calling `session.bind_workspace(...)` (around line 1124).
- **Critical invariant:** Refresh `cwd` on every bind and update workspace/git metadata without discarding the transcript or breaking persisted-session compatibility.
- **Narrow validation command:** `cargo test -p ocean-agent`.

### 2. Add/fix Anthropic/OpenAI/Gemini/Codex wire encoding
- **Owner repo/crate:** `risingtides-dev/ocean-os` / `ocean-protocol`.
- **Primary entry file/symbol:** `crates/ocean-protocol/src/providers/` (provider-specific encoder/stream modules; facade `src/lib.rs`).
- **Critical invariant:** Provider quirks remain isolated; Codex bound turns use the stable Ocean session id for both `prompt_cache_key` and HTTP `session_id`.
- **Narrow validation command:** `cargo test -p ocean-protocol`.

### 3. Filesystem/bash tool runs in wrong cwd or bypasses permission
- **Owner repo/crate:** `risingtides-dev/ocean-os` / `ocean-runtime`.
- **Primary entry file/symbol:** `crates/ocean-runtime/src/agent_loop.rs` — tool permission/dispatch loop; cwd originates at `capability.rs::SessionContext`.
- **Critical invariant:** Every execution path passes the permission gate, and relative filesystem/process operations resolve against the turn's `SessionContext.cwd`, never daemon cwd.
- **Narrow validation command:** `cargo test -p ocean-runtime`.

### 4. Add a TUI keybinding/shared `Action` variant
- **Owner repo/crate:** `risingtides-dev/ocean-os` / `ocean-tui`.
- **Primary entry file/symbol:** `crates/ocean-tui/src/shell/action.rs::Action`.
- **Critical invariant:** Variants are additive, and all mutation flows through component-emitted `Action` values consumed by `App::dispatch`/component `update`.
- **Narrow validation command:** `cargo test -p ocean-tui && cargo build -p ocean-tui --release` (also `cargo check --workspace --tests` when the enum is shared).

### 5. Add/change daemon HTTP/SSE route with caller cwd semantics
- **Owner repo/crate:** `risingtides-dev/ocean-os` / `ocean-daemon`.
- **Primary entry file/symbol:** `crates/ocean-daemon/src/main.rs` — `Router::new().route(...)` assembly (around line 1577) and route handler.
- **Critical invariant:** Effective cwd comes from client cwd/project metadata and never falls back to daemon process cwd; session authority remains in `ocean-agent`.
- **Narrow validation command:** `cargo test -p ocean-daemon`.

### 6. Change PSTN/LiveKit call pipeline
- **Owner repo/crate:** `risingtides-dev/ocean-os` / `ocean-call` for PSTN/Twilio and call-intelligence runtime; coordinate `risingtides-dev/ocean-surface` for LiveKit presentation/client surface changes.
- **Primary entry file/symbol:** `crates/ocean-call/src/session_task.rs` — session task/`FrameSource` loop and `live::LiveKitFrameSource`/`LiveKitVoice` adapters.
- **Critical invariant:** `ocean-call` owns the audio/call pipeline but not daemon HTTP routes; surface presentation must remain a thin daemon client.
- **Narrow validation command:** `cargo test -p ocean-call`.

### 7. Change Longhouse quorum/title/revocation behavior
- **Owner repo/crate:** `risingtides-dev/ocean-os` / `ocean-longhouse` for local coordination logic; `risingtides-dev/ocean-bedrock` owns shared data-plane support.
- **Primary entry file/symbol:** `crates/ocean-longhouse/src/quorum.rs` (quorum), `src/escrow.rs` (title/escrow/recall paths), facade `src/lib.rs`.
- **Critical invariant:** Longhouse coordination must not become daemon permission/execution authority, and shared records remain Bedrock-owned.
- **Narrow validation command:** `cargo test -p ocean-longhouse`.

### 8. Add an external MCP tool versus subprocess plugin tool
- **Owner repo/crate:** External server/tool: `risingtides-dev/ocean-os` / `ocean-mcp`; subprocess manifest/JSON-RPC tool: `risingtides-dev/ocean-os` / `ocean-plugin`.
- **Primary entry file/symbol:** External MCP: `crates/ocean-mcp/src/provider.rs`; subprocess plugin: `crates/ocean-plugin/src/plugin.rs`.
- **Critical invariant:** Do not conflate external MCP transport with subprocess plugin lifecycle/protocol; execution still respects runtime capability/permission boundaries.
- **Narrow validation command:** `cargo test -p ocean-mcp` or `cargo test -p ocean-plugin`, matching the chosen tool type.

### 9. Change handoff extraction versus hashline edits versus AST summarization
- **Owner repo/crate:** `risingtides-dev/ocean-os` / respectively `ocean-context`, `ocean-hashline`, `ocean-ast`.
- **Primary entry file/symbol:** `crates/ocean-context/src/extract.rs`; `crates/ocean-hashline/src/patcher.rs`; `crates/ocean-ast/src/lib.rs::summarize_code`.
- **Critical invariant:** Keep evidence-bearing handoff extraction, file-hash-anchored mutation/stale recovery, and read-only structural summarization separate; `ocean-ast` is not live-runtime wired.
- **Narrow validation command:** Respectively `cargo test -p ocean-context`, `cargo test -p ocean-hashline`, or `cargo test -p ocean-ast`.

### 10. Add/validate a workspace package including non-default members
- **Owner repo/crate:** `risingtides-dev/ocean-os` / root Cargo workspace plus canonical `crates/AGENTS.md` index (package itself is its own boundary).
- **Primary entry file/symbol:** root `Cargo.toml` — `[workspace].members` / `default-members`; `crates/AGENTS.md::Workspace Package Index`.
- **Critical invariant:** Add/remove/rename changes update the canonical index and match `cargo metadata`; explicitly validate non-default `ocean-ast` and `xtask` or use `--workspace`.
- **Narrow validation command:** `cargo metadata --no-deps --format-version=1` (then the package row's test command; `cargo test --workspace` includes non-default members).

## Architecture

Clients route through `ocean-daemon`, which owns HTTP/SSE and composes `ocean-agent` session authority with `ocean-runtime` permission-gated execution. Provider wire behavior is isolated in `ocean-protocol`. Specialized package boundaries (`call`, `longhouse`, `mcp`, `plugin`, `context`, `hashline`, `ast`) prevent similarly named concerns from collapsing together. Cross-repo UI/shared-data concerns route to `ocean-surface` and `ocean-bedrock` respectively.

**Deep source search required:** cases 1, 3, 5, and 6, because the two canonical routing docs identified crate/file ownership but not a sufficiently precise operational symbol. Cases 2, 4, and 7-10 were answerable from the root/index plus linked local contracts/project map without deeper source reading.

**Total cases answered:** 10/10.

**Ambiguity:** Case 6 depends on whether “LiveKit” means runtime audio adapters (`ocean-call`) or client presentation (`ocean-surface`). Case 7 similarly splits local Longhouse behavior (`ocean-longhouse`) from shared data-plane persistence (`ocean-bedrock`). Case 8 requires choosing transport type before selecting its one owner. No other material routing ambiguity found.

## Start Here

Open `crates/AGENTS.md` first: it is the canonical ownership/entry/validation index and resolves most cases without source search.

```acceptance-report
{
  "criteriaSatisfied": [
    {
      "id": "criterion-1",
      "status": "satisfied",
      "evidence": "Completed the requested read-only 10-case routing benchmark without repository edits or prohibited-doc access."
    },
    {
      "id": "criterion-2",
      "status": "satisfied",
      "evidence": "Each case includes owner, primary entry, invariant, and narrow validation; retrieved paths and deep-search cases are enumerated."
    }
  ],
  "changedFiles": [],
  "testsAddedOrUpdated": [],
  "commandsRun": [
    {
      "command": "read AGENTS.md and crates/AGENTS.md",
      "result": "passed",
      "summary": "Read canonical routing contracts first."
    },
    {
      "command": "read linked crate AGENTS.md files and docs/OCEAN_PROJECT_MAP.md",
      "result": "passed",
      "summary": "Resolved local and cross-repo contracts without reading prohibited paths."
    },
    {
      "command": "targeted grep in ocean-agent, ocean-daemon, ocean-runtime, and ocean-call sources",
      "result": "passed",
      "summary": "Located precise operational symbols for cases 1, 3, 5, and 6."
    }
  ],
  "validationOutput": [
    "10 of 10 cases answered.",
    "No repository files modified; no tests were run because this was a read-only benchmark."
  ],
  "residualRisks": [
    "Case 6 ownership depends on whether the requested LiveKit change is runtime audio or surface presentation.",
    "Case 7 ownership depends on whether behavior is local coordination or shared Bedrock data-plane persistence."
  ],
  "noStagedFiles": true,
  "diffSummary": "No repository diff; wrote only the requested report under /tmp.",
  "reviewFindings": [
    "no blockers"
  ],
  "manualNotes": "The /tmp report artifact is intentionally excluded from changedFiles because it is outside the repository."
}
```
