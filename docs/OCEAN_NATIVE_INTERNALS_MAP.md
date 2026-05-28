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

## Planned future seams

| Future crate | Pulls out of | Why |
|---|---|---|
| `ocean-store` | `ocean-agent` session JSON load/save | SQLite-backed session/event store, off the filesystem |
| `ocean-tools` | `ocean-runtime::tools` | Stand alone so plugin runtimes can target a stable tool ABI |
| `ocean-plugin` | new | WASM/subprocess plugin host |

## Smoke contract

The end-to-end smoke test that must keep passing through any internal restructure:

```bash
ocean-daemon &
ocean-rs prompt "Reply exactly: OCEAN_OK"
# expect: OCEAN_OK
```
