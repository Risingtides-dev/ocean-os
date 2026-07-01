# ocean-rs roadmap

## Phase 0: daemon shell
- [x] Name/runtime: ocean-rs
- [x] Workspace scaffold
- [x] Local daemon API
- [x] CLI client
- [x] Temporary pi-rs-deepseek backend adapter
- [x] Remove external pi-rs-deepseek process from daemon prompt path
- [x] Add in-process ocean-agent crate for DeepSeek-backed prompt runs

## Phase 1: real daemon protocol
- [x] WebSocket/SSE streaming endpoint
- [x] Persistent sessions
- [x] Request queue and cancellation
- [x] OceanTUI integration
- [ ] Ocean GUI integration

## Phase 2: native agent runtime
- [x] First daemon-owned agent facade
- [x] Daemon-safe non-interactive permission default
- [x] Ocean-owned tool registry
- [x] Approval protocol for mutating tools
- [ ] Native DeepSeek provider crate
- [x] OpenAI-compatible provider abstraction
- [x] Native read/write/edit/bash tools

## Phase 3: runtime + surface integration
- [x] Longhouse engine recovered — daemon-computed quorum + convene flow (OCEAN-9)
- [x] Gemini/Google provider routing (OCEAN-10)
- [x] `POST /v1/agent/sessions` — create-before-turn session allocation (OCEAN-11)
- [x] Per-turn wall-clock timeout (OCEAN-17)
- [x] Session registry GC + real session detail/list fields (OCEAN-12/13/19)
- [x] Session-scoped SSE — `?session_id=` filtering for first-party surfaces (OCEAN-15)
- [x] Extension-event scoping — Extension/Longhouse events scoped, Invariant 5 exception documented (OCEAN-56)
- [x] Per-session / per-turn model override — independent windows pin their own model (OCEAN-36/46)
- [x] Per-turn `thinking_level` — per-turn reasoning-effort override without mutating global state (OCEAN-28/41/35)
- [x] Surface-extension system prompt — `surface-extension` arm in `append_client_type` (OCEAN-14/16/50)
- [x] Persistent `Room` struct — durable rooms + read-only browser tool perms + MCP per-session unblock (OCEAN-20/44/39)
- [x] Longhouse topics endpoints — `GET /v1/longhouse/topics` + `/topics/{id}`, claim_outcome quorum gate (OCEAN-58/59)
- [x] Call pipeline — inbound+outbound Twilio/LiveKit call-intelligence service (`ocean-call`)
- [x] Browser control surface — CDP-driven Chrome tool suite (Layer-3 input,
      live network capture, downloads, tab shell)
- [x] ACP bridge — Ocean daemon exposed to Zed and other ACP editors
- [x] MCP client — external MCP servers wired in as agent tools

## Phase 4: OS integration
- [ ] systemd user service
- [ ] Unix socket
- [ ] desktop notifications
- [ ] distro config defaults
- [ ] sandbox profiles

## Phase 5: extensibility
- [x] Subprocess plugins — `ocean-plugin` crate; tools reach live turns via
      `PluginProvider` registration in `build_capability_registry`
      (`crates/ocean-agent/src/lib.rs:1612`) (OCEAN-95)
- [ ] WASM plugins via wasmtime
- [ ] Skill/prompt packs
- [ ] Theme/client protocol

## Built, pending daemon integration — RESOLVED

Everything this section used to track as built-but-not-wired has since shipped
into the live daemon path. Re-verified against source on main, 2026-07-01;
anchors below. See `docs/ARCHITECTURE.md` § "Shipped since the original
integration list" for operator impact.

- [x] `ocean-store` (SqliteRoomStore) wired — the daemon opens the durable
      store at startup (`crates/ocean-daemon/src/main.rs:1612`) and holds it on
      `AppState.rooms` (main.rs:88, constructed at main.rs:1639); rooms and
      transcripts survive restart (OCEAN-86/107)
- [x] `PluginProvider` registered — `build_capability_registry`
      (`crates/ocean-agent/src/lib.rs:1546`) calls `discover_plugin_providers`
      (lib.rs:1612) and registers each `ocean_plugin::PluginProvider`
      (connected at lib.rs:1693) (OCEAN-95)
- [x] Room auto-convene queues a real agent turn — `room_post_message` calls
      `spawn_room_agent_turn` (`crates/ocean-daemon/src/main.rs:6517`, defined
      at main.rs:6642) when a mention resolves to a roster `Agent`
      (OCEAN-111/128/225)
- [x] ACP permission forwarding active — ACP turns run gated by default
      (`yolo_enabled()`, OCEAN-51) and `run_turn`
      (`crates/ocean-acp/src/main.rs:710`) subscribes the control stream
      before `submit_turn` (main.rs:742-785), so `spawn_permission_bridge`
      (main.rs:973) delivers editor-side approval prompts (OCEAN-146)
- [x] Cross-provider `Content::Image` — encoded on all four provider wire
      paths under `crates/ocean-protocol/src/providers/`: anthropic.rs:159,194;
      openai.rs:218,306; google.rs:140,244; codex.rs:66,154
      (OCEAN-99/131/132/133)
- [x] Longhouse escrow trio on AppState — persisted `SqliteTitleRegistry`
      opened at startup (`crates/ocean-daemon/src/main.rs:1628`) and held on
      `AppState` with the `Revoker` and quorum-of-recall registry
      (main.rs:1640-1642); `/v1/longhouse/revoke` and `/v1/longhouse/recall`
      routes live (OCEAN-229/246/272/302)

Still genuinely open:

- [ ] Longhouse validator/staking *economics* — the escrow ledger mechanics
      exist (`crates/ocean-longhouse/src/escrow.rs`) but the economic policy
      (stake sizing, forfeiture schedule) is not designed
      (see `docs/LONGHOUSE.md` § "Built vs unbuilt")
