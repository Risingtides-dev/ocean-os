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
- [ ] WebSocket/SSE streaming endpoint
- [ ] Persistent sessions
- [ ] Request queue and cancellation
- [ ] OceanTUI integration
- [ ] Ocean GUI integration

## Phase 2: native agent runtime
- [x] First daemon-owned agent facade
- [x] Daemon-safe non-interactive permission default
- [ ] Ocean-owned tool registry
- [ ] Approval protocol for mutating tools
- [ ] Native DeepSeek provider crate
- [ ] OpenAI-compatible provider abstraction
- [ ] Native read/write/edit/bash tools

## Phase 3: OS integration
- [ ] systemd user service
- [ ] Unix socket
- [ ] desktop notifications
- [ ] distro config defaults
- [ ] sandbox profiles

## Phase 4: extensibility
- [ ] Subprocess plugins
- [ ] WASM plugins via wasmtime
- [ ] Skill/prompt packs
- [ ] Theme/client protocol
