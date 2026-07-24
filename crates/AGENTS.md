# crates/ — Canonical Rust Workspace Index

## Purpose

This child doc governs `crates/` and is the canonical ownership, entry-point, and validation index for every Ocean OS workspace package. Root/bootstrap docs point here instead of maintaining competing crate inventories.

## Ownership

- **Scope:** `crates/` plus the root `xtask` workspace package
- **Parent contract:** `../AGENTS.md` — read it first
- **Primary owner:** Rust workspace maintainers and agents editing crate source

## Local Contracts


- Treat each package as an ownership boundary.
- Read the target package's local `AGENTS.md` when the index links one.
- Do not introduce cross-crate coupling without documenting the contract in affected owner docs.
- When adding/removing/renaming a workspace package, update this index in the same change and verify it against `cargo metadata --no-deps --format-version=1`.
- Keep entry points and narrow validation current; stale routing information is a correctness defect for agent work.
- Agent turns are session/workspace scoped and do not carry a Track-0 `room_id`. Durable collaboration uses `RoomKey` and `/v1/rooms/persistent/*`; LiveKit token minting remains independent at `/v1/rooms/{room_id}/livekit-token`.
- The current desktop product client is `surface-tauri`; removed client
  identifiers and parallel desktop prompt families must not be restored.
- Persistent room transcript rows and their per-room `seq` are the only durable room-event log. Store participant-mutation adapters return their committed join/leave marker for post-commit daemon wake publication; do not add a parallel room-events table or treat the bounded wake channel as authority.
- Subagent definitions and orchestration are extension-owned. Core crates may provide generic permission-gated execution/capability seams, but must not own named subagent roles, spawn/join policy, worker budgets, or orchestration schedulers.
- Feature credentials remain isolated from agent routing: xAI speech uses `xai`,
  OpenAI Realtime voice uses `openai-realtime`, and future embedding providers
  require dedicated blocks plus a live typed consumer before picker exposure.
- The Chromium browser backend is quarantined behind the default-off
  `legacy-chromium` feature chain (`ocean-browser` → `ocean-runtime` →
  `ocean-agent` → `ocean-daemon`) while it is replaced by the OceanWebKit
  browser host. Default builds compile no chromiumoxide; the 19 `browser_*`
  tools keep their exact schemas but return `browser_host_unavailable`, and
  `/v1/browser/screencast` + `/v1/browser/input` serve the frozen `no-browser`
  contract from `browser_stream_stub.rs`. Validate both modes:
  `cargo check --workspace` and `cargo clippy -p ocean-daemon --features legacy-chromium -- -D warnings` (both are CI lanes).

## Workspace Package Index

The workspace currently contains 30 Rust packages.

| Package | Owns | Does not own | Primary entry | Local contract | Narrow validation |
|---|---|---|---|---|---|
| `ocean-acp` | ACP stdio bridge for Zed/editors | Runtime/session authority | `ocean-acp/src/main.rs`, `daemon.rs` | — | `cargo test -p ocean-acp` |
| `ocean-agent` | Sessions/history, prompt assembly, capability/runtime facade | Provider wire encoding; client UI | `ocean-agent/src/lib.rs`, `ocean-agent/src/session/mod.rs`, `ocean-agent/src/system_prompt.rs` | `ocean-agent/AGENTS.md` | `cargo test -p ocean-agent` |
| `ocean-agent-sdk` | Product session/turn/event/surface vocabulary | Daemon execution and persistence | `ocean-agent-sdk/src/lib.rs` | — | `cargo test -p ocean-agent-sdk` |
| `ocean-ast` | Standalone tree-sitter read-time structural summarization | Handoff extraction; hashline mutation; live runtime wiring | `ocean-ast/src/lib.rs::summarize_code` | — | `cargo test -p ocean-ast` |
| `ocean-browser` | Legacy Chrome DevTools handle, launch, tabs, perception, network/downloads (feature-gated, default-off `legacy-chromium`; replaced by the OceanWebKit browser host) | Runtime permission policy and tool registration | `ocean-browser/src/lib.rs` | — | `cargo test -p ocean-browser --features legacy-chromium` |
| `ocean-call` | PSTN/Twilio/LiveKit audio and call-intelligence pipeline | Daemon HTTP route ownership | `ocean-call/src/lib.rs`, `session_task.rs` | — | `cargo test -p ocean-call` |
| `ocean-cli` | Thin daemon command/prompt/session client | Agent loop and session persistence | `ocean-cli/src/main.rs` | — | `cargo test -p ocean-cli` |
| `ocean-context` | Evidence-bearing handoff claims, extraction, replay, reverification | Agent sessions; hashline edits; general AST summaries | `ocean-context/src/lib.rs`, `extract.rs` | — | `cargo test -p ocean-context` |
| `ocean-core` | Low-level daemon request/response/event/session protocol types | Product-facing agent SDK behavior | `ocean-core/src/lib.rs` | `ocean-core/AGENTS.md` | `cargo test -p ocean-core` |
| `ocean-daemon` | HTTP/SSE service, per-turn harness-profile composition, execution authority, bounded public/read-only GitHub repository projection, room access/outbox HTTP projection and retry adapter, outbound Bedrock room client/supervisor, P2-C local/federated intent and invite/member routes, sovereign bound-agent trigger dispatch, observatory auth extractor | Provider implementation; client-owned state; GitHub authentication/writes/aggregation; room access/outbox/federation/pending-redemption durability and trigger claims (owned by ocean-store); auth logic (owned by ocean-observatory) | `ocean-daemon/src/main.rs`, `github.rs`, `room_federation.rs`, `persistent_rooms.rs`, `harness_profile.rs`, `observatory_auth.rs` | `ocean-daemon/AGENTS.md` | `cargo test -p ocean-daemon` |
| `ocean-extension` | Schema-v1 extension package parsing, SemVer compatibility, and confined resource validation | Install/trust/enable state; routes; execution | `ocean-extension/src/lib.rs` | `ocean-extension/AGENTS.md` | `cargo test -p ocean-extension` |
| `ocean-hashline` | File-hash-anchored surgical edits and stale recovery | General AST summarization or session persistence | `ocean-hashline/src/lib.rs`, `patcher.rs` | — | `cargo test -p ocean-hashline` |
| `ocean-heartbeat` | Scheduled/routine CLI that calls the daemon | In-daemon scheduling authority | `ocean-heartbeat/src/main.rs` | — | `cargo test -p ocean-heartbeat` |
| `ocean-hooks` | Plugin-agnostic subprocess lifecycle hooks | Plugin/MCP tool protocols | `ocean-hooks/src/lib.rs` | — | `cargo test -p ocean-hooks` |
| `ocean-longhouse` | Correlation-aware sequential evidence, pure review planning, legacy net-weight quorum, council/convene, titles, escrow, recall/revocation, deterministic exact-token and inspectable skills/workflow preparation, advisory spec assembly | Daemon execution authority; extension-owned subagent dispatch/orchestration | `ocean-longhouse/src/lib.rs`, `evidence.rs`, `quorum.rs`, `planner.rs`, `convene.rs`, `prepare.rs`, `escrow.rs` | — | `cargo test -p ocean-longhouse` |
| `ocean-lsp` | Language-server clients, discovery, diagnostics ledger, `lsp` tool | General AST parsing or editor UI | `ocean-lsp/src/lib.rs`, `tool.rs` | `ocean-lsp/AGENTS.md` | `cargo test -p ocean-lsp` |
| `ocean-mcp` | Client connections to external MCP servers and tool adapters | Ocean MCP server; subprocess plugins | `ocean-mcp/src/lib.rs`, `provider.rs` | — | `cargo test -p ocean-mcp` |
| `ocean-memory` | Typed provenance-bearing SQLite memory and ingest | Session transcripts; shared Bedrock storage | `ocean-memory/src/lib.rs` | — | `cargo test -p ocean-memory` |
| `ocean-minimizer` | Standalone conservative output minimization for already-tokenized cargo/git/gh/npm/npx/pytest invocations | Shell parsing/execution; TOML/config; artifacts; live runtime wiring | `ocean-minimizer/src/lib.rs::minimize` | `ocean-minimizer/AGENTS.md` | `cargo test -p ocean-minimizer && cargo clippy -p ocean-minimizer --all-targets -- -D warnings` |
| `ocean-oauth` | Browser OAuth/PKCE login and Ocean auth-file writes | Model routing and provider wire calls | `ocean-oauth/src/lib.rs` | `ocean-oauth/AGENTS.md` | `cargo test -p ocean-oauth` |
| `ocean-observatory` | Metadata-only event schema, structural redaction allow-list, cursor semantics, SQLite/WAL persistence, scoped observer auth (HMAC-SHA256), boot-bound mode-0600 token-file distribution, extension admission validation (cycle/depth/cross-authority/idempotency), and in-memory single-use binding tokens | Daemon event adapters or Surface rendering | `ocean-observatory/src/lib.rs`, `ocean-observatory/src/auth.rs`, `ocean-observatory/src/admission.rs` | — | `cargo test -p ocean-observatory` |
| `ocean-plugin` | Subprocess plugin manifests, lifecycle, JSON-RPC, capability adapter | External MCP transport | `ocean-plugin/src/lib.rs`, `plugin.rs` | — | `cargo test -p ocean-plugin` |
| `ocean-protocol` | Anthropic/OpenAI/Gemini/Codex wire encoding, streaming, retry | Model catalog, credentials, readiness | `ocean-protocol/src/lib.rs`, `providers/` | `ocean-protocol/AGENTS.md` | `cargo test -p ocean-protocol` |
| `ocean-providers` | Model catalog/routing, credentials, aliases, readiness | Provider request/stream encoding | `ocean-providers/src/lib.rs` | — | `cargo test -p ocean-providers` |
| `ocean-runtime` | Agent loop, permission gates, cancellation, capability/tool execution, runtime events | Session persistence; model credential routing | `ocean-runtime/src/lib.rs`, `agent_loop.rs` | `ocean-runtime/AGENTS.md` | `cargo test -p ocean-runtime` |
| `ocean-search` | Standalone bounded typed in-memory and trusted-root filesystem byte search over `ocean-walker` | Live runtime grep/glob, path authorization, capability/profile/tool wiring | `ocean-search/src/lib.rs::search_bytes`, `search_path` | `ocean-search/AGENTS.md` | `cargo test -p ocean-search && cargo clippy -p ocean-search --all-targets -- -D warnings` |
| `ocean-store` | SQLite durable rooms, rosters, transcripts behind `RoomStore`; inherent access-projection and outbox APIs; restart-safe federation core (credentials, bindings, producer counters, confirmed ingest, trigger-claim journal) | Agent sessions, memory, Longhouse titles; federation network client | `ocean-store/src/lib.rs` | `ocean-store/AGENTS.md` | `cargo test -p ocean-store` |
| `ocean-tui` | Ratatui steering cockpit and client interaction | Agent/session/runtime authority | `ocean-tui/src/main.rs`, `shell/` | `ocean-tui/AGENTS.md` | `cargo test -p ocean-tui && cargo build -p ocean-tui --release` |
| `ocean-walker` | Standalone native filesystem traversal, filtering, parallel candidate delivery, and TTL scan caching | Typed content search; live runtime grep/glob or capability wiring | `ocean-walker/src/lib.rs::WalkRequest`, `walk_entries`, `collect_entries` | `ocean-walker/AGENTS.md` | `cargo test -p ocean-walker && cargo clippy -p ocean-walker --all-targets -- -D warnings` |
| `xtask` | Repository docs/index checks, canonical repository/compatibility/MSRV gate manifests, WebRTC-cache recovery | Production runtime behavior | `../xtask/src/main.rs` | `../xtask/README.md` | `cargo test -p xtask && cargo xtask docs-check && cargo xtask ci --compatibility` |

## Non-default Members

- `ocean-ast` is standalone and not yet wired into the live runtime. It stays outside `default-members` to avoid adding its multi-grammar compile cost to ordinary default builds; validate it explicitly or through `--workspace`.
- `ocean-minimizer` is a standalone, dependency-free M1 library and is not yet wired into command capture or a harness profile. It stays outside `default-members` so ordinary builds do not imply live minimization; validate it explicitly or through `--workspace`.
- `ocean-walker` is a standalone M1 traversal/cache library used by standalone `ocean-search` but not wired into live runtime grep/glob. It stays outside `default-members` so ordinary builds do not imply production filesystem-tool adoption; validate it explicitly or through `--workspace`.
- `ocean-search` is a standalone M1 typed-search library over trusted roots and is not wired into runtime grep/glob, capabilities, profiles, or sessions. It stays outside `default-members` so ordinary builds do not imply live search adoption; validate it explicitly or through `--workspace`.
- `xtask` is a developer task runner, not a product binary. Invoke it explicitly with `cargo xtask <command>`; `cargo xtask ci` owns the executable local/CI gate manifest while workspace commands still build/test xtask through `--workspace`.

## Cross-crate Change Impact

| Change | Read/coordinate first | Required fanout |
|---|---|---|
| Shared request/session serde | defining owner in `ocean-core` or `ocean-agent-sdk`, plus `ocean-agent`, daemon, TUI/ACP consumers | Owner tests + `cargo check --workspace --tests` |
| Runtime `AgentEvent` / product turn-event bridge | `ocean-runtime`, `ocean-agent`, `ocean-agent-sdk`, daemon, TUI/ACP consumers | Runtime/SDK/agent tests + `cargo check --workspace --tests` |
| Agent session persistence/rebind | `ocean-agent`, daemon | `cargo test -p ocean-agent`; daemon tests; workspace gate |
| Tool execution/permissions/cwd/cancellation | runtime, agent, daemon | Runtime E2E/permission tests; daemon tests; workspace gate |
| Model catalog/routing | providers, protocol, agent, TUI | `cargo test -p ocean-providers`; protocol tests; workspace tests |
| Provider wire/streaming | protocol provider module | Focused fixtures + `cargo test -p ocean-protocol` |
| HTTP/SSE route | daemon plus core/SDK consumers | Narrow route tests + daemon tests + client compile/tests |
| TUI shared enum/render/event flow | TUI plus event owner | TUI tests/release build; workspace tests for shared enums |
| Persistence schema | owning store/memory/context/Longhouse package | Migration/backward-compat and restart-persistence tests |


## Work Guidance

- Prefer small, explicit package boundaries.
- Run the narrowest package check first, then the root completion/merge gate.
- Session/history changes usually cross `ocean-agent`, `ocean-core`, and `ocean-daemon`; coordinate explicitly.
- Public picker models are owned by `ocean-providers::known_models`; every advertised id must round-trip through `resolve_model_selection`, and every routable production alias must be listed. Kimi K3 is the exact `kimi-k3` route with a 1M context window; its provider-specific dynamic-tool wire belongs to `ocean-protocol`, while bounded search/dispatch enforcement belongs to `ocean-runtime`.

## Verification

- Index parity: compare this table with `cargo metadata --no-deps --format-version=1`.
- Package logic: run the row's narrow command and the nearest local contract checks.
- Repo-wide completion: follow `../AGENTS.md`, including compatibility and MSRV lanes when build/dependency contracts change.

## Child devlog Index

- `ocean-agent/` — session/history layer and system prompt loading → `ocean-agent/AGENTS.md`
- `ocean-core/` — shared protocol types → `ocean-core/AGENTS.md`
- `ocean-daemon/` — long-running HTTP daemon and API surface → `ocean-daemon/AGENTS.md`
- `ocean-extension/` — non-executing extension package schema validation → `ocean-extension/AGENTS.md`
- `ocean-lsp/` — code intelligence over workspace language servers → `ocean-lsp/AGENTS.md`
- `ocean-minimizer/` — standalone conservative command-output minimization → `ocean-minimizer/AGENTS.md`
- `ocean-oauth/` — browser OAuth + PKCE provider login → `ocean-oauth/AGENTS.md`
- `ocean-protocol/` — multi-provider LLM wire protocol → `ocean-protocol/AGENTS.md`
- `ocean-runtime/` — agent loop and permission-gated tool execution → `ocean-runtime/AGENTS.md`
- `ocean-search/` — standalone bounded typed byte/path search → `ocean-search/AGENTS.md`
- `ocean-store/` — SQLite durable rooms and federation store → `ocean-store/AGENTS.md`
- `ocean-tui/` — terminal steering cockpit → `ocean-tui/AGENTS.md`
- `ocean-walker/` — standalone native filesystem traversal and scan caching → `ocean-walker/AGENTS.md`
