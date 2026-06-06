# Ocean native internals dependency map

## Scope

The runtime stack is now fully Ocean-owned, in-tree:

```text
ocean-agent      session/history layer
  └─ ocean-runtime    agent loop + tools (read/write/edit/bash/grep/glob/ls/web_fetch/todo)
       └─ ocean-protocol    multi-provider LLM wire protocol (Anthropic/OpenAI/Google/OAI-compat)
            └─ ocean-providers   model routing + credential resolution
```

All four crates live under `crates/` and are edited as a single Rust workspace. The previous pi-* runtime/protocol crates have been replaced by in-tree equivalents.

## What `ocean-agent` owns directly

- request/session normalization
- daemon-safe backend naming (`ocean-native-deepseek`)
- config-dir resolution and DeepSeek auth fallback
- session file load/save/list logic
- prompt-to-response shaping for the HTTP API
- fallback extraction of the last assistant text for smoke behavior
- permission policy wiring to `ocean-runtime`

## Extracted seams (built)

These were once planned future seams and now exist as workspace crates:

| Crate | Path | State |
|---|---|---|
| `ocean-store` | `crates/ocean-store` | Built. SQLite-backed (`rusqlite`, bundled) durable storage. Ships `SqliteRoomStore` — a synchronous `RoomStore`-trait implementation that mirrors the in-memory `RoomRegistry` in `ocean-agent` (`crates/ocean-agent/src/rooms.rs`) method-for-method, so the two are interchangeable behind a `dyn RoomStore`. |
| `ocean-plugin` | `crates/ocean-plugin` | Built. Subprocess-first plugin runtime for agent skill packs. Ships the `Plugin` trait, `PluginManifest` (`plugin.toml` parser), and `SubprocessPlugin` (JSON-RPC 2.0 over stdio, mirroring `ocean-mcp`'s transport without depending on it). Behind the on-by-default `runtime` feature it exposes `PluginProvider`, a `CapabilityProvider` adapter that composes plugin tools into the same `CapabilityRegistry` as built-ins and MCP tools. |

## Planned future seams

| Future crate | Pulls out of | Why |
|---|---|---|
| `ocean-tools` | `ocean-runtime::tools` | [Not yet built — roadmap] Stand alone so plugin runtimes can target a stable tool ABI |

## Smoke contract

The end-to-end smoke test that must keep passing through any internal restructure:

```bash
ocean-daemon &
ocean-rs prompt "Reply exactly: OCEAN_OK"
# expect: OCEAN_OK
```
