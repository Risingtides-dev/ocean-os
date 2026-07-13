# Code Context

## Files Retrieved
1. `Cargo.toml` (lines 1-55) - workspace members/default-members, including non-default `ocean-ast` and `xtask`.
2. `crates/ocean-agent/AGENTS.md` (lines 1-32) - session/history ownership and workspace-rebind invariant.
3. `crates/ocean-protocol/AGENTS.md` (lines 1-35) - provider wire ownership and Codex/Anthropic invariants.
4. `crates/ocean-runtime/AGENTS.md` (lines 1-42) - permission, cwd, tool scheduling, and hashline-editor contracts.
5. `crates/ocean-daemon/AGENTS.md` (lines 1-31) - HTTP/SSE ownership and effective-cwd contract.
6. `crates/ocean-tui/AGENTS.md` (lines 1-79) - shared Action/Elm-loop rules and validation requirements.
7. `crates/ocean-agent/src/lib.rs` (search hits around lines 1099-1174, 1249-1283, 1552, 2619-2669) - workspace binding, history loading/persistence, and `bind_workspace`.
8. `crates/ocean-protocol/src/providers/{anthropic,openai,google,codex}.rs` - provider-specific wire implementations discovered from the source tree.
9. `crates/ocean-runtime/src/agent_loop.rs` (search hits around lines 389-458) and `crates/ocean-runtime/src/tools/bash.rs` (around line 99) - permission-before-execution and command cwd.
10. `crates/ocean-tui/src/shell/action.rs` (symbol at line 31) - shared `Action` enum.
11. `crates/ocean-daemon/src/main.rs` (route table lines 1577-1701; SSE around 2247-2311; call path around 4701-5218; Longhouse router around 3101-3138) - daemon route composition and integrations.
12. `crates/ocean-call/src/{sip_bridge,session_task,room_tap,orchestrator}.rs` - call pipeline modules discovered from the source tree.
13. `crates/ocean-longhouse/src/{quorum,escrow,convene}.rs` - quorum and persisted-title/revocation implementation; `escrow.rs` lines 1-52 states the authority model.
14. `crates/ocean-mcp/src/provider.rs` (lines 1-431 by targeted search hits) - `McpProvider`/`McpTool` adapter and external-tool permission default.
15. `crates/ocean-plugin/src/{plugin,provider}.rs` (targeted search hits through lines 1-106) - `Plugin`, `SubprocessPlugin`, and `PluginProvider` seam.
16. `crates/ocean-context/src/extract.rs` (lines 1-47) and related `claim.rs`/`store.rs` search hits - deterministic handoff claim extraction and storage.

## Key Code

1. **owner repo/crate:** `ocean-os / ocean-agent`; **primary entry file/symbol:** `crates/ocean-agent/src/lib.rs` — `Session::bind_workspace` and turn history load/persist path; **one critical invariant:** rebinding refreshes cwd/workspace metadata without replacing or discarding the existing session transcript; **narrow validation command:** `cargo test -p ocean-agent bind_workspace`.
2. **owner repo/crate:** `ocean-os / ocean-protocol`; **primary entry file/symbol:** `crates/ocean-protocol/src/providers/{anthropic.rs,openai.rs,google.rs,codex.rs}` — provider request/stream encoders; **one critical invariant:** provider quirks remain isolated and Codex bound turns use the stable Ocean session id for both `prompt_cache_key` and HTTP `session_id`; **narrow validation command:** `cargo test -p ocean-protocol`.
3. **owner repo/crate:** `ocean-os / ocean-runtime`; **primary entry file/symbol:** `crates/ocean-runtime/src/agent_loop.rs` — permission dispatch, plus `tools/bash.rs` execution; **one critical invariant:** filesystem/process execution resolves against `SessionContext.cwd` and cannot execute after a denied permission decision; **narrow validation command:** `cargo test -p ocean-runtime`.
4. **owner repo/crate:** `ocean-os / ocean-tui`; **primary entry file/symbol:** `crates/ocean-tui/src/shell/action.rs:Action`; **one critical invariant:** add variants without removing/renaming existing ones, and route mutation through the Elm `Action` dispatch/update loop; **narrow validation command:** `cargo check -p ocean-tui --tests`.
5. **owner repo/crate:** `ocean-os / ocean-daemon`; **primary entry file/symbol:** `crates/ocean-daemon/src/main.rs` — `Router::new().route(...)`, turn handlers, and SSE handlers; **one critical invariant:** effective cwd comes from caller cwd/project metadata and never daemon process cwd, including resumed turns; **narrow validation command:** `cargo test -p ocean-daemon`.
6. **owner repo/crate:** `ocean-os / ocean-call` (daemon adapter in `ocean-daemon`); **primary entry file/symbol:** `crates/ocean-call/src/sip_bridge.rs:LiveKitSipBridge` and `session_task.rs` live session task; **one critical invariant:** live dialing remains credential/config gated and webhook verification failure must not trigger call-side effects; **narrow validation command:** `cargo test -p ocean-call`.
7. **owner repo/crate:** `ocean-os / ocean-longhouse`; **primary entry file/symbol:** `crates/ocean-longhouse/src/quorum.rs:QuorumEngine` and `escrow.rs:{SqliteTitleRegistry,Revoker}`; **one critical invariant:** only a live, token-verified persisted title may authorize a claim; revoked/released titles never authorize even with the old token; **narrow validation command:** `cargo test -p ocean-longhouse`.
8. **owner repo/crate:** `ocean-os / ocean-mcp` for external MCP, `ocean-os / ocean-plugin` for subprocess plugins; **primary entry file/symbol:** `crates/ocean-mcp/src/provider.rs:McpProvider` versus `crates/ocean-plugin/src/provider.rs:PluginProvider` / `plugin.rs:SubprocessPlugin`; **one critical invariant:** both adapt through runtime `CapabilityProvider`, but MCP names are collision-safe (`mcp__<server>__<tool>`) and external mutations remain permission-gated; **narrow validation command:** `cargo test -p ocean-mcp && cargo test -p ocean-plugin`.
9. **owner repo/crate:** `ocean-os / ocean-context` (handoff extraction), `ocean-hashline` (surgical edits), `ocean-ast` (AST summaries); **primary entry file/symbol:** `crates/ocean-context/src/extract.rs:extract_claims`, with the respective hashline and AST crate entry libraries; **one critical invariant:** these are separate concerns—deterministic prose extraction must not be conflated with source mutation or persisted AST/signature summarization; **narrow validation command:** `cargo test -p ocean-context -p ocean-hashline -p ocean-ast`.
10. **owner repo/crate:** `ocean-os / root Cargo workspace`; **primary entry file/symbol:** `Cargo.toml:[workspace].members` and `default-members`; **one critical invariant:** every package belongs in `members`, while packages intentionally excluded from defaults (currently `ocean-ast` and `xtask`) still compile under explicit workspace validation; **narrow validation command:** `cargo check --workspace`.

## Architecture

Clients enter through daemon routes; the daemon delegates transcript/workspace state to `ocean-agent`, tool execution and permission enforcement to `ocean-runtime`, and provider serialization to `ocean-protocol`. The TUI is only a client/interaction shell. Specialized capability crates (`ocean-mcp`, `ocean-plugin`) adapt tools into the runtime seam. Calls and Longhouse have domain crates with daemon route adapters. Context extraction, hashline editing, and AST summarization are deliberately separate crates.

**Total cases answered:** 10/10.

**Ambiguity encountered:** Case 8 necessarily has two owners because “external MCP” and “subprocess plugin” are distinct integration paths. Case 9 likewise spans three intentionally separate crates. Case 6’s domain owner is `ocean-call`, while HTTP route/task wiring is in `ocean-daemon`. No forbidden plan/archive files were read.

## Start Here

Open `crates/ocean-agent/src/lib.rs` first for the reported cold-agent baseline’s highest-risk cross-client issue: it contains bind/rebind plus transcript load/persist flow.

```acceptance-report
{
  "criteriaSatisfied": [
    {
      "id": "criterion-1",
      "status": "satisfied",
      "evidence": "Answered all 10 routing cases without modifying repository files or widening scope."
    },
    {
      "id": "criterion-2",
      "status": "satisfied",
      "evidence": "Each case includes owner, entry file/symbol, invariant, and narrow validation command, backed by retrieved source/contracts."
    }
  ],
  "changedFiles": [],
  "testsAddedOrUpdated": [],
  "commandsRun": [
    {
      "command": "targeted find/grep/read inspection of Cargo.toml, crate AGENTS.md files, and crate source",
      "result": "passed",
      "summary": "Located ownership boundaries, entry symbols, invariants, and test commands without reading forbidden paths."
    }
  ],
  "validationOutput": [
    "Static routing baseline completed: 10/10 cases answered.",
    "No repository validation commands were executed because this was a read-only scouting task."
  ],
  "residualRisks": [
    "Narrow test-name discovery was intentionally stopped after sufficient evidence; some recommended commands validate the whole owning crate rather than one named regression.",
    "No tests were executed, so commands are routing recommendations rather than observed pass results."
  ],
  "noStagedFiles": true,
  "diffSummary": "No repository diff; only the required /tmp report was written.",
  "reviewFindings": [
    "no blockers"
  ],
  "manualNotes": "Read-only task. Forbidden docs/.agentarchive and readiness-plan paths were not read. noStagedFiles reflects that no repository files were edited or staged; git status was not invoked."
}
```
