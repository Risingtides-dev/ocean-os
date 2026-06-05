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
