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
- [ ] Subprocess plugins
- [ ] WASM plugins via wasmtime
- [ ] Skill/prompt packs
- [ ] Theme/client protocol

## Built, pending daemon integration

These exist in the workspace (crate compiles, unit-tested) but the daemon does
not yet construct/register them, so the **feature is not live**. See
`docs/ARCHITECTURE.md` § "Built, pending daemon integration" for operator impact.

- [ ] Wire `ocean-store` (SqliteRoomStore) into the daemon — persistent rooms
      are currently held in the in-memory `RoomRegistry` and LOST on restart
      (OCEAN-86 built the store; daemon still uses the in-memory registry)
- [ ] Register `PluginProvider` in `build_capability_registry` — installed
      plugins contribute zero tools to a turn until then (OCEAN-95 built the
      provider; the daemon never constructs it)
- [ ] Queue a real agent turn on room auto-convene — `room_post_message`
      evaluates the trigger policy and emits a `room_trigger` notice, but does
      not yet spawn a turn for the mentioned agent (OCEAN-65; held behind the
      in-flight `agent_turn` permission PRs)
- [ ] Activate ACP permission forwarding — the per-turn permission bridge in
      `ocean-acp` is built but inert because the daemon submits ACP turns with
      `yolo: true` (it gates the moment ACP turns run non-yolo; OCEAN-51 / #54)
- [ ] Cross-provider `Content::Image` — produced (browser `perceive`) and wired
      for Anthropic, but the OpenAI/Gemini encoders drop image content
- [ ] Longhouse governance layer (escrow trio: TitleRegistry + Revoker +
      validator escrow + unforgeable `claim_outcome` gate) — quorum steps 1–5
      are built; steps 6+ are stubbed (see `docs/LONGHOUSE.md`)
