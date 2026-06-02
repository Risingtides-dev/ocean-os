# Ocean OS — read this first

**Longhouse note:** Ocean Longhouse is the hive — the local-first agentic operations hub where agents go before they act. It centralizes SOPs, routines/workflows, tools/MCP discovery, skills, memory/knowledge, subagent specs, and quorum/council workflows. `ocean-daemon` remains the local runtime/body and execution authority; Longhouse coordinates/recommends and must not bypass daemon permission gates. Canonical doc: `docs/LONGHOUSE.md`.

**This repo is one half of a two-repo system. The other half is `ocean-surface`.**

| Repo | What it is | Where |
|---|---|---|
| **ocean-os** (you are here) | The **runtime + daemon + TUI**. Owns the agent loop, tools, provider calls, **sessions**, permissions, events. This is the brain. | `../ocean-os` |
| **ocean-surface** | The **client face** (browser PWA + voice, later Tauri native). A thin steering shell — holds **no** agent logic, **no** sessions. | `../ocean-surface` (also cloned locally) |

If someone mentions "ocean-surface", "the web surface", "the voice client", or "Leo" — **it is not a foreign country.** It is the sibling repo at `../ocean-surface`, already cloned on this machine. Go read it before saying you don't know what it is.

## How the two talk

Both clients (the TUI in this repo, and ocean-surface) steer the **same daemon** over one HTTP+SSE API:

```
POST /v1/agent/turns    { prompt, cwd, session_id?, guidance?, room_id? }
GET  /v1/agent/events   (SSE stream of AgentTurnEvent)
```

The daemon listens on `127.0.0.1:4780` by default (`OCEAN_BIND` to override).

**Sessions live HERE, in the daemon, only.** Clients just carry a `session_id` string and replay it on each turn. So any "lost session / restarted mid-chat" bug is almost always a daemon-side session bug in this repo (`crates/ocean-agent` session load/save), and it breaks **both** clients at once — because they both depend on the daemon remembering the transcript by id.

## Crate map

| Crate | Role |
|---|---|
| `ocean-core` | Shared protocol types: requests, responses, events, sessions |
| `ocean-protocol` | Multi-provider LLM wire protocol (Anthropic, OpenAI, Gemini, OpenAI-compatible) |
| `ocean-runtime` | Agent loop + permission-gated tool execution |
| `ocean-providers` | Provider registry: model routing, credentials, readiness |
| `ocean-agent` | **Session/history layer** wrapping the runtime — session load/save lives here |
| `ocean-agent-sdk` | SDK surface for embedding the agent in other Rust code |
| `ocean-daemon` | Long-running HTTP service on `:4780`. Owns runtime authority |
| `ocean-cli` (`ocean-rs`) | CLI client |
| `ocean-tui` (`ocean`) | Terminal steering cockpit |

## Build / run

```bash
cargo build --workspace --release
./target/release/ocean-daemon              # the daemon
./target/release/ocean-tui                 # or: ocean (symlinked release binary)
```

The `ocean` binary is a symlink to the release build — after any TUI change, run `cargo build -p ocean-tui --release`.

## Don't kill a running daemon

The daemon is often live while the operator works. **Do not restart or kill it** to apply a fix unless explicitly told to — source fixes don't touch the running process, and a surprise restart drops whatever session is in flight. Stage the fix, tell the operator, let them restart.

## More context

- Architecture: `docs/ARCHITECTURE.md`
- Repo routing / Linear rules: `docs/CLAUDE.md`, `docs/AGENTS.md`
- Operator guide: `docs/OCEAN_RUNTIME_OPERATOR_GUIDE.md`
