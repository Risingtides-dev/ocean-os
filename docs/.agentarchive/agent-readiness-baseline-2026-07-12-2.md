# Code Context

## Files Retrieved
1. `crates/ocean-agent/src/lib.rs` (lines 1216-1552, 4574-4735) - session/history load, serialization, and rebind preservation.
2. `crates/ocean-daemon/src/main.rs` (lines 1577-1675, 8928-9026, 9319-9496, 15428-15492) - HTTP router and caller-cwd/session-rebind semantics.
3. `crates/ocean-protocol/src/providers/{anthropic,openai,google,codex}.rs` - provider-specific wire adapters.
4. `crates/ocean-runtime/src/agent_loop.rs` (lines 389-465) and `crates/ocean-runtime/src/tools/{bash,path}.rs` (bash lines 20-98; path lines 3-14) - permission gate and cwd-bound execution.
5. `crates/ocean-tui/src/main.rs` (lines 258 onward, 2788-2832) - shared `Action` enum and key-event mapping.
6. `crates/ocean-call/src/{orchestrator,sip_bridge,room_tap,session_task}.rs` - PSTN/LiveKit call pipeline boundaries.
7. `crates/ocean-longhouse/src/{quorum,convene,escrow}.rs` (notably `escrow.rs` lines 20-52, 151-204) - convergence, title authority, and revocation.
8. `crates/ocean-mcp/src/provider.rs` (lines 36-239) and `crates/ocean-plugin/src/plugin.rs` (lines 1-90) - external MCP and subprocess-plugin tool seams.
9. `crates/ocean-context/src/{claim,extract,treesitter}.rs`, `crates/ocean-hashline/src/patcher.rs` (lines 61-91), and `crates/ocean-ast/src/summary.rs` (lines 15-147) - three distinct context/edit/summarization concerns.
10. `Cargo.toml` (lines 1-58) - workspace members/default-members, including non-default `ocean-ast` and `xtask`.

## Key Code

1. **Owner:** `ocean-os` / `ocean-agent`; **primary entry:** `crates/ocean-agent/src/lib.rs` — session request path around history clone/persist and rebind tests; **critical invariant:** moving a session to a newly bound workspace must preserve the canonical message history intact; **narrow validation:** `cargo test -p ocean-agent rebind`.
2. **Owner:** `ocean-os` / `ocean-protocol`; **primary entry:** `crates/ocean-protocol/src/providers/{anthropic.rs,openai.rs,google.rs,codex.rs}` — provider request/response encoders; **critical invariant:** each provider’s internal `Message`/tool-call representation must round-trip to that provider’s exact wire schema without conflating Codex with generic OpenAI; **narrow validation:** `cargo test -p ocean-protocol providers`.
3. **Owner:** `ocean-os` / `ocean-runtime`; **primary entry:** `crates/ocean-runtime/src/tools/bash.rs::BashTool::for_cwd` and `agent_loop.rs` permission phase; **critical invariant:** relative execution resolves against the turn-bound cwd, and a permission-denied tool slot never executes; **narrow validation:** `cargo test -p ocean-runtime bash`.
4. **Owner:** `ocean-os` / `ocean-tui`; **primary entry:** `crates/ocean-tui/src/main.rs::Action` plus the key-event-to-Action mapping; **critical invariant:** input mapping emits the shared semantic `Action` and handling remains exhaustive/consistent across modes; **narrow validation:** `cargo test -p ocean-tui key`.
5. **Owner:** `ocean-os` / `ocean-daemon`; **primary entry:** `crates/ocean-daemon/src/main.rs` `Router::new()` route table and `agent_turn`/`resolve_bound_cwd`; **critical invariant:** caller-declared cwd is traversal-guarded and a resumed turn rebinds to the caller’s requested cwd while retaining its session; **narrow validation:** `cargo test -p ocean-daemon resumed_turn_rebinds`.
6. **Owner:** `ocean-os` / `ocean-call` (daemon integration is in `ocean-daemon`); **primary entry:** `crates/ocean-call/src/orchestrator.rs` with `sip_bridge.rs` and `room_tap.rs`; **critical invariant:** PSTN setup, LiveKit room/token participation, and audio task lifecycle stay one consistently identified call session; **narrow validation:** `cargo test -p ocean-call`.
7. **Owner:** `ocean-os` / `ocean-longhouse`; **primary entry:** `crates/ocean-longhouse/src/quorum.rs::QuorumEngine` and `escrow.rs::{SqliteTitleRegistry,Revoker}`; **critical invariant:** only live, correctly verified server-issued titles can authorize convergence, and revoked/released titles cannot authorize even with the old token; **narrow validation:** `cargo test -p ocean-longhouse`.
8. **Owner:** external MCP → `ocean-os` / `ocean-mcp`; subprocess plugin → `ocean-os` / `ocean-plugin`; **primary entry:** `ocean-mcp/src/provider.rs::McpProvider::connect` versus `ocean-plugin/src/plugin.rs::Plugin`/`SubprocessPlugin`; **critical invariant:** MCP tools are discovered and namespaced `mcp__<server>__<tool>`, whereas plugin tools honor the transport-independent `Plugin` contract—do not implement one through the other; **narrow validation:** `cargo test -p ocean-mcp && cargo test -p ocean-plugin`.
9. **Owner:** extraction → `ocean-os` / `ocean-context`; hashline edits → `ocean-os` / `ocean-hashline`; AST summarization → `ocean-os` / `ocean-ast`; **primary entry:** `ocean-context/src/extract.rs`, `ocean-hashline/src/patcher.rs::apply_patch`, `ocean-ast/src/summary.rs::summarize_code`; **critical invariant:** codified handoff claims remain evidence-bearing context, hashline patches fail safely on anchor mismatch, and AST summarization preserves source order/structure—these are separate seams; **narrow validation:** `cargo test -p ocean-context && cargo test -p ocean-hashline && cargo test -p ocean-ast`.
10. **Owner:** `ocean-os` workspace root / target package crate; **primary entry:** root `Cargo.toml` `[workspace].members` and `default-members`; **critical invariant:** every package is a workspace member and explicit workspace validation includes non-default members such as `ocean-ast` and `xtask`; **narrow validation:** `cargo check --workspace`.

## Architecture

Protocol adapters feed the permission-gated runtime; `ocean-agent` wraps that runtime with serialized session/history state; `ocean-daemon` exposes it over cwd-aware HTTP/SSE routes; the TUI is a daemon client. Call and Longhouse are domain crates integrated by the daemon. MCP and plugins are parallel tool-extension mechanisms. Context extraction, hashline editing, and AST summarization are deliberately separate crates. The root Cargo workspace is the only complete package-validation boundary.

## Start Here

Open `crates/ocean-daemon/src/main.rs` first for cases 1 and 5 because it binds caller cwd, session identity, and routes; then follow session persistence into `crates/ocean-agent/src/lib.rs`. For all other cases, start at the primary entry named above.

**Total cases answered:** 10/10.

**Ambiguity encountered:** Cases 6, 8, and 9 intentionally span ownership seams. Case 6’s core pipeline belongs to `ocean-call` but HTTP/startup integration belongs to `ocean-daemon`; case 8 names two distinct extension systems; case 9 names three distinct crates. Validation commands were selected from code/test naming evidence but were not executed under the cold-routing/read-only constraint.

```acceptance-report
{
  "criteriaSatisfied": [
    {
      "id": "criterion-1",
      "status": "satisfied",
      "evidence": "Completed the requested read-only cold-agent routing baseline for all 10 cases without modifying repository files or reading excluded paths."
    },
    {
      "id": "criterion-2",
      "status": "satisfied",
      "evidence": "Each case includes owner, primary entry file/symbol, critical invariant, and narrow validation command, backed by exact retrieved paths and line ranges where available."
    }
  ],
  "changedFiles": [],
  "testsAddedOrUpdated": [],
  "commandsRun": [],
  "validationOutput": [
    "Static inspection answered 10 of 10 routing cases.",
    "No repository validation commands were executed; commands above are recommended narrow checks."
  ],
  "residualRisks": [
    "Some suggested cargo test filters may match zero tests if test names have changed; use the crate-wide command shown for cases without a stable filter.",
    "No runtime validation was performed."
  ],
  "noStagedFiles": true,
  "diffSummary": "No repository diff; wrote only the required /tmp scouting artifact.",
  "reviewFindings": [
    "no blockers"
  ],
  "manualNotes": "Excluded docs/.agentarchive and docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md were not read."
}
```
