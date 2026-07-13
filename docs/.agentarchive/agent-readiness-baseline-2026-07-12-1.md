# Code Context

## Files Retrieved
1. `Cargo.toml` (lines 1-60) — workspace membership and `default-members` boundary.
2. `crates/ocean-daemon/src/main.rs` (search evidence around lines 1577-1620, 8928-9005, 9319-9335, 15414-15492) — router, caller-cwd/session binding, and resume tests.
3. `crates/ocean-protocol/src/lib.rs` (lines 1-57) and `crates/ocean-protocol/src/providers/openai.rs` (search evidence throughout provider implementation/tests) — provider dispatch and wire encoding.
4. `crates/ocean-runtime/src/capability.rs` (lines 34-42, 227-290, 385-446) — cwd rebinding, built-in tool precedence, permission delegation.
5. `crates/ocean-tui/src/main.rs` (lines 258 onward; 1730-1738; 2788-2832) — shared `Action` enum and key mapping.
6. `crates/ocean-call/src/session_task.rs` (lines 22-82, 1186-1397) — testable call loop and LiveKit adapters.
7. `crates/ocean-longhouse/src/quorum.rs` (lines 183-191, 375, 459-525) and `registry.rs` (lines 11-15, 140-220) — convergence, title recall, durable event projection.
8. `crates/ocean-mcp/src/{client,config,provider,transport}.rs` and `crates/ocean-plugin/src/{plugin,provider,subprocess,transport}.rs` — distinct external MCP and subprocess-plugin stacks.
9. `crates/ocean-context/src/extract.rs` (lines 1-47), `treesitter.rs` (lines 380, 647 onward), plus `crates/ocean-hashline/src/{hash,format,normalize}.rs` — separate handoff extraction, AST, and hashline domains.

## Key Code

1. **Resumed history after rebind** — owner: **ocean-os / `ocean-daemon` + `ocean-agent`**; primary: `crates/ocean-daemon/src/main.rs::agent_turn` / `resolve_bound_cwd` (history persistence is the `ocean-agent` session layer); invariant: a known session is strictly resumed with its persisted transcript while execution cwd may rebind only through the traversal/workspace guard; validate: `cargo test -p ocean-daemon resumed_turn_rebinds_when_workspace_changes`.
2. **Anthropic/OpenAI/Gemini/Codex encoding** — owner: **ocean-os / `ocean-protocol`**; primary: `crates/ocean-protocol/src/lib.rs::stream` dispatch, then `src/providers/{anthropic,openai,google,codex}.rs`; invariant: provider-specific request and streaming response shapes normalize to shared protocol events without silently dropping tool/image/thinking content; validate: `cargo test -p ocean-protocol`.
3. **Filesystem/bash cwd or permission** — owner: **ocean-os / `ocean-runtime`**; primary: `crates/ocean-runtime/src/capability.rs::resolve_capabilities` (built-ins use `*Tool::for_cwd`); invariant: every cwd-sensitive built-in is reconstructed with turn cwd and permission wrappers delegate `requires_permission`; external providers cannot shadow built-in `bash`; validate: `cargo test -p ocean-runtime capability`.
4. **TUI keybinding/shared Action** — owner: **ocean-os / `ocean-tui`**; primary: `crates/ocean-tui/src/main.rs::Action` and key-event-to-action mapping; invariant: input mapping emits the shared Action and behavior is handled centrally rather than mutating state in the key decoder; validate: `cargo test -p ocean-tui`.
5. **Daemon HTTP/SSE route + caller cwd** — owner: **ocean-os / `ocean-daemon`**; primary: `crates/ocean-daemon/src/main.rs` router construction near `Router::new` and target handler (`agent_turn` for cwd semantics); invariant: resumed turns run in caller-requested cwd only after binding/traversal validation, while SSE preserves event ordering/resume contract; validate: `cargo test -p ocean-daemon`.
6. **PSTN/LiveKit call pipeline** — owner: **ocean-os / `ocean-call`** (daemon wires it); primary: `crates/ocean-call/src/session_task.rs::run_session` pipeline and `live` adapters; invariant: core FrameSource/STT/Agent/Voice loop remains testable without native LiveKit, with native adapters gated by `livekit-tap`; validate: `cargo test -p ocean-call`.
7. **Longhouse quorum/title/revocation** — owner: **ocean-os / `ocean-longhouse`**; primary: `crates/ocean-longhouse/src/quorum.rs::QuorumEngine` and recall tally; invariant: only QuorumEngine decides convergence and revocation occurs only after a carried genuine quorum using server-minted authority; registry merely projects events; validate: `cargo test -p ocean-longhouse`.
8. **External MCP vs subprocess plugin** — owner: **ocean-os / `ocean-mcp` for MCP; `ocean-plugin` for subprocess plugins**; primary: `ocean-mcp/src/provider.rs` versus `ocean-plugin/src/subprocess.rs`/`provider.rs`; invariant: use MCP transport/config for external MCP servers, plugin manifest/process lifecycle for Ocean plugins, while capability dedup prevents either from overriding built-ins; validate: `cargo test -p ocean-mcp -p ocean-plugin`.
9. **Handoff extraction vs hashline vs AST summary** — owner: **ocean-os / `ocean-context`, `ocean-hashline`, `ocean-ast` respectively**; primary: `ocean-context/src/extract.rs::extract_claims`; `ocean-hashline/src/format.rs`/hash APIs; `ocean-context/src/treesitter.rs` or `ocean-ast/src/lib.rs` for structural summaries; invariant: deterministic prose claim extraction, content-derived hashline patch integrity, and stable tree-sitter symbol/signature shapes are separate contracts; validate: `cargo test -p ocean-context -p ocean-hashline -p ocean-ast`.
10. **Workspace package/non-default member** — owner: **ocean-os workspace root / target crate**; primary: root `Cargo.toml` `[workspace].members` and `default-members`; invariant: every package is in `members`, while intentionally non-default packages (currently `ocean-ast` and `xtask`) must still compile under explicit workspace/package validation; validate: `cargo check --workspace`.

## Architecture
The daemon owns HTTP/SSE, session routing, and caller-cwd binding; `ocean-agent` persists transcripts; `ocean-runtime` builds the permission-gated, cwd-bound tool set; `ocean-protocol` handles provider wire formats. TUI and call surfaces invoke daemon/runtime contracts. Longhouse, MCP/plugin, context/hashline/AST are bounded specialist crates integrated through daemon/runtime providers.

## Start Here
Open `crates/ocean-daemon/src/main.rs` first for cases 1 and 5 because it joins session persistence, workspace rebinding, routes, and SSE. For all other cases, begin at the crate-specific primary file listed above.

Total cases answered: **10/10**.

Ambiguity encountered: case 9 says “AST summarization”; evidence spans `ocean-context::treesitter` (handoff anchor/signature stability) and the separate `ocean-ast` crate, so ownership depends on whether the requested summary is handoff-context-specific or the general AST tool. Case 1 crosses daemon routing and agent persistence rather than belonging to one crate exclusively.

```acceptance-report
{
  "criteriaSatisfied": [
    {"id":"criterion-1","status":"satisfied","evidence":"Read-only cold-routing baseline answered all 10 requested cases without widening scope or reading excluded paths."},
    {"id":"criterion-2","status":"satisfied","evidence":"Each case includes owner, primary entry, invariant, and narrow validation command with retrieved file evidence."}
  ],
  "changedFiles": [],
  "testsAddedOrUpdated": [],
  "commandsRun": [
    {"command":"targeted find/grep/read inspection only","result":"passed","summary":"Mapped workspace and ten requested ownership domains; no build/tests executed because task requested routing baseline and stop after enough evidence."}
  ],
  "validationOutput": ["10/10 cases answered; output artifact written to /tmp/ocean-agent-readiness-baseline-1.md"],
  "residualRisks": ["Validation commands are recommended narrow checks and were not executed.","Case 9 ownership is context-dependent as noted."],
  "noStagedFiles": true,
  "diffSummary": "No repository changes; one required /tmp markdown artifact created.",
  "reviewFindings": ["no blockers"],
  "manualNotes": "Excluded docs/.agentarchive and the named readiness plan were not read."
}
```
