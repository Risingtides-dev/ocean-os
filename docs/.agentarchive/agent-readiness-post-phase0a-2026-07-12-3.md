# Code Context

## Files Retrieved
1. `AGENTS.md` (root contract; routing, workspace gates, daemon/session invariants) - repository authority.
2. `crates/AGENTS.md` (lines 15-91) - canonical package ownership, entries, narrow checks, fanout, and non-default members.
3. `crates/ocean-agent/AGENTS.md` (lines 15-30) - rebind/session invariants.
4. `crates/ocean-protocol/AGENTS.md` (local contracts and verification) - wire compatibility invariants.
5. `crates/ocean-runtime/AGENTS.md` (lines 15-28) - cwd and permission invariants.
6. `crates/ocean-tui/AGENTS.md` (lines 15-40) - additive `Action` and Elm-loop rules.
7. `crates/ocean-daemon/AGENTS.md` (lines 15-32) - HTTP/SSE caller-cwd rules.
8. `docs/OCEAN_PROJECT_MAP.md` (routing tables and shared contracts) - checked cross-repo ownership, especially call/LiveKit and Longhouse boundaries.

No deeper source search was required for any case. The requested plan and `docs/.agentarchive` were not read.

## Key Code

### 1. Resumed session loses history after workspace rebind
- **Owner repo/crate:** `ocean-os` / `ocean-agent` (coordinate `ocean-daemon`).
- **Primary entry file/symbol:** `crates/ocean-agent/src/lib.rs`.
- **One critical invariant:** Every bind refreshes recorded `cwd`; when workspace changes, update `workspace_root` and git metadata without losing compatible session history.
- **Narrow validation command:** `cargo test -p ocean-agent`.

### 2. Add/fix Anthropic/OpenAI/Gemini/Codex wire encoding
- **Owner repo/crate:** `ocean-os` / `ocean-protocol`.
- **Primary entry file/symbol:** `crates/ocean-protocol/src/providers/`.
- **One critical invariant:** Provider-specific quirks stay behind protocol abstractions and must not leak into shared `ocean-core` types.
- **Narrow validation command:** `cargo test -p ocean-protocol`.

### 3. Filesystem/bash tool runs in wrong cwd or bypasses permission
- **Owner repo/crate:** `ocean-os` / `ocean-runtime`.
- **Primary entry file/symbol:** `crates/ocean-runtime/src/agent_loop.rs`.
- **One critical invariant:** Filesystem/process tools must pass mandatory permission gates and resolve relative paths/commands against the turn's `SessionContext.cwd`, never daemon process cwd.
- **Narrow validation command:** `cargo test -p ocean-runtime`.

### 4. Add a TUI keybinding/shared `Action` variant
- **Owner repo/crate:** `ocean-os` / `ocean-tui`.
- **Primary entry file/symbol:** `crates/ocean-tui/src/shell/action.rs::Action`.
- **One critical invariant:** `Action` changes are additive; components emit actions and mutation occurs only through `App::dispatch`/component `update`.
- **Narrow validation command:** `cargo test -p ocean-tui && cargo build -p ocean-tui --release`.

### 5. Add/change daemon HTTP/SSE route with caller cwd semantics
- **Owner repo/crate:** `ocean-os` / `ocean-daemon`.
- **Primary entry file/symbol:** `crates/ocean-daemon/src/main.rs`.
- **One critical invariant:** Effective cwd comes from client cwd/project metadata and never falls back to daemon process cwd or a resumed session's first cwd.
- **Narrow validation command:** `cargo test -p ocean-daemon`.

### 6. Change PSTN/LiveKit call pipeline
- **Owner repo/crate:** `ocean-os` / `ocean-call`.
- **Primary entry file/symbol:** `crates/ocean-call/src/session_task.rs`.
- **One critical invariant:** `ocean-call` owns PSTN/Twilio/LiveKit audio and call intelligence, but daemon HTTP routes remain owned by `ocean-daemon` (surface owns only presentation).
- **Narrow validation command:** `cargo test -p ocean-call`.

### 7. Change Longhouse quorum/title/revocation behavior
- **Owner repo/crate:** `ocean-os` / `ocean-longhouse`.
- **Primary entry file/symbol:** `crates/ocean-longhouse/src/quorum.rs` (with `lib.rs`/`escrow.rs` for title/escrow/revocation boundaries).
- **One critical invariant:** Local/runtime coordination logic belongs here; shared Longhouse data-plane support belongs to `ocean-bedrock`, and daemon execution authority does not move into this crate.
- **Narrow validation command:** `cargo test -p ocean-longhouse`.

### 8. Add an external MCP tool versus subprocess plugin tool
- **Owner repo/crate:** external MCP: `ocean-os` / `ocean-mcp`; subprocess plugin: `ocean-os` / `ocean-plugin`.
- **Primary entry file/symbol:** MCP: `crates/ocean-mcp/src/provider.rs`; plugin: `crates/ocean-plugin/src/plugin.rs`.
- **One critical invariant:** External MCP transport/adapters and subprocess manifest/lifecycle/JSON-RPC are separate ownership boundaries; do not implement one through the other.
- **Narrow validation command:** `cargo test -p ocean-mcp && cargo test -p ocean-plugin`.

### 9. Change handoff extraction versus hashline edits versus AST summarization
- **Owner repo/crate:** `ocean-os` / `ocean-context` vs `ocean-hashline` vs `ocean-ast`.
- **Primary entry file/symbol:** `crates/ocean-context/src/extract.rs`; `crates/ocean-hashline/src/patcher.rs`; `crates/ocean-ast/src/lib.rs::summarize_code`.
- **One critical invariant:** Evidence-bearing handoff extraction, file-hash-anchored mutation, and read-only structural summarization remain separate; `ocean-ast` is not live-runtime wired.
- **Narrow validation command:** `cargo test -p ocean-context && cargo test -p ocean-hashline && cargo test -p ocean-ast`.

### 10. Add/validate a workspace package including non-default members
- **Owner repo/crate:** `ocean-os` / workspace (`crates/AGENTS.md` plus root `Cargo.toml`; `xtask` is also in scope).
- **Primary entry file/symbol:** `Cargo.toml` workspace `members`/`default-members` and `crates/AGENTS.md::Workspace Package Index`.
- **One critical invariant:** Adding/removing/renaming a package updates the canonical index in the same change; non-default members such as `ocean-ast` and `xtask` must be validated explicitly or via `--workspace`.
- **Narrow validation command:** `cargo metadata --no-deps --format-version=1`.

## Architecture

Routing is package-boundary first: daemon routes into agent session authority and runtime tool authority; protocol owns provider wire encoding; clients such as the TUI remain thin. Specialized packages (`call`, `longhouse`, `mcp`, `plugin`, `context`, `hashline`, `ast`) have deliberately exclusive ownership. Cross-crate enum/API/session changes require the fanout checks listed in `crates/AGENTS.md`, beyond the narrow first command above.

**Total cases answered:** 10/10.

**Ambiguity:** Low. Case 6 can become cross-repo if the requested change is LiveKit UI/presentation (`ocean-surface`), but the stated PSTN/LiveKit call pipeline routes to `ocean-call`. Case 7 can become cross-repo when changing shared Longhouse storage/data-plane behavior (`ocean-bedrock`). Case 10's exact package entry symbol cannot exist until the package is named; metadata/index parity is nevertheless unambiguous.

## Start Here

Open `crates/AGENTS.md` first: it is the canonical package ownership, entry-point, exclusion, fanout, and narrow-validation index for all ten cases.

```acceptance-report
{
  "criteriaSatisfied": [
    {
      "id": "criterion-1",
      "status": "satisfied",
      "evidence": "Completed the bounded 10-case cold-agent routing benchmark without modifying repository files or reading excluded paths."
    },
    {
      "id": "criterion-2",
      "status": "satisfied",
      "evidence": "Each case includes owner, primary entry, one invariant, and narrow validation; retrieved files and line ranges are identified."
    }
  ],
  "changedFiles": [],
  "testsAddedOrUpdated": [],
  "commandsRun": [
    {
      "command": "git status --porcelain && nl -ba crates/AGENTS.md ... (targeted line-number inspection of local contracts)",
      "result": "passed",
      "summary": "Confirmed routing evidence and observed pre-existing dirty worktree state."
    }
  ],
  "validationOutput": [
    "10/10 cases answered; no deep source search required.",
    "Repository already contained modified and untracked files; this task made no repository changes."
  ],
  "residualRisks": [
    "Case 6 ownership depends on whether a future request targets call runtime or surface presentation.",
    "Case 7 shared data-plane changes require ocean-bedrock coordination."
  ],
  "noStagedFiles": true,
  "diffSummary": "No repository diff created; only /tmp/ocean-agent-readiness-post-3.md was written.",
  "reviewFindings": [
    "no blockers"
  ],
  "manualNotes": "No tests were run because this was a read-only routing benchmark. Excluded documents were not read."
}
```
