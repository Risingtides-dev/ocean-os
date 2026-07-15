# ocean-acp

An **Agent Client Protocol (ACP)** bridge that exposes the Ocean daemon to
[Zed](https://zed.dev) (and any other ACP editor — Neovim, Emacs, …) as a
first-class agent.

```
  Zed ──ACP / stdio (newline JSON-RPC)──▶ ocean-acp ──HTTP + SSE──▶ ocean-daemon (:4780)
```

The bridge holds **no agent logic and no sessions of its own**. It translates
ACP requests into calls against the daemon's existing session, model, turn,
permission, cancellation, and SSE APIs, then streams daemon events back as ACP
`session/update` notifications. All authority — the agent loop, tools, sessions,
the transcript — stays in the daemon, exactly like the TUI and ocean-surface
clients.

This is the inverse of `ocean-mcp` (which makes Ocean an MCP *client*); here
Ocean is an ACP *agent/server*.

## What works

| ACP method | Mapped to |
|---|---|
| `initialize` | Negotiates the requested protocol version and advertises session load/list capabilities |
| `session/new` | Creates a daemon session up front, returns its id as the ACP id, remembers the daemon-bound `cwd`, and advertises the live model roster as ACP modes |
| `session/load` | Looks up the daemon session, restores its authoritative `cwd`, rebinds the persisted transcript, and refreshes model modes |
| `session/list` | Lists daemon-owned sessions with optional `cwd` filtering and cursor pagination |
| `session/set_mode` | Pins an Ocean model to that ACP session and sends it as a per-turn override, without mutating the daemon's global model |
| `session/prompt` | Submits `POST /v1/agent/turns`, pumps SSE to `session/update`, bridges permission requests, and returns the mapped `stopReason` |
| `session/cancel` | Cancels the active daemon turn through `POST /v1/requests/{turn_id}/cancel` |

Ocean also accepts an optional per-prompt reasoning override in
`_meta.ocean.thinking_level`. Without it, the daemon's configured reasoning
level remains authoritative.

Daemon `AgentTurnEvent`s map to ACP updates like so:

| Daemon event | ACP `session/update` |
|---|---|
| `assistant_text_delta` | `agent_message_chunk` |
| `thinking_delta` | `agent_thought_chunk` |
| `tool_call_started` | `tool_call` (kind inferred from tool name) |
| `tool_call_chunk` / `tool_call_finished` | `tool_call_update` |
| `component_render` (kanban/table/…) | Markdown summary in `agent_message_chunk` |
| `turn_finished` | resolves the prompt with a `stopReason` |

### Session id mapping (why it matters)

ACP needs a `sessionId` returned at `session/new`, before any turn. Ocean now
creates the daemon session at that point with `POST /v1/agent/sessions` and
returns the daemon's id directly as the ACP id. The editor therefore persists
the same identifier the daemon owns:

- `session/new` creates and binds the daemon session to the requested workspace.
- The first and later `session/prompt` calls submit that id, so every turn uses
  the same daemon-owned transcript.
- `session/load` validates the id against the daemon, restores the daemon-bound
  `cwd`, and resumes the persisted transcript even after `ocean-acp` restarts.
- `session/list` exposes the daemon's persisted roster rather than an
  in-process-only bridge list.

If the daemon cannot create a session during `session/new`, the bridge remains
available by falling back to a local ACP id. On that legacy fallback path it
learns the daemon id from the first turn's event stream, but the in-memory
mapping cannot survive a bridge restart. If a later `session/load` cannot find
the session in the daemon, the bridge restores a usable `cwd` and starts a fresh
daemon session instead of pretending the old transcript was resumed.

## Build

```bash
cargo build -p ocean-acp --release
# → target/release/ocean-acp
```

## Use it in Zed

**1. The Ocean daemon must be running** (the bridge talks to it, it does not
start it):

```bash
./target/release/ocean-daemon
```

**2. Add the agent to your Zed `settings.json`** (`cmd-,` in Zed) under
`agent_servers`:

```json
{
  "agent_servers": {
    "Ocean": {
      "type": "custom",
      "command": "/Users/risingtidesdev/dev/ocean-os/target/release/ocean-acp",
      "args": [],
      "env": {}
    }
  }
}
```

**3.** Open the Zed agent panel and pick **Ocean** from the `+` (new thread)
menu. It behaves like any built-in agent.

### Options

The bridge takes one optional flag / env var:

| Flag | Env | Default |
|---|---|---|
| `--daemon-url <url>` | `OCEAN_ACP_DAEMON_URL` | `http://127.0.0.1:4780` |

Logs go to **stderr** (stdout is the ACP JSON-RPC channel and stays clean).
Set verbosity with `OCEAN_ACP_LOG` (e.g. `OCEAN_ACP_LOG=debug`). Zed surfaces
the agent's stderr in its logs, which is handy while debugging.

To point Zed at a daemon on another host/port:

```json
{
  "agent_servers": {
    "Ocean": {
      "type": "custom",
      "command": "/Users/risingtidesdev/dev/ocean-os/target/release/ocean-acp",
      "args": ["--daemon-url", "http://127.0.0.1:4780"],
      "env": { "OCEAN_ACP_LOG": "info" }
    }
  }
}
```

## Manual smoke test (no editor needed)

ACP is newline-delimited JSON-RPC over stdio, so you can drive it by hand:

```bash
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}' \
  '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"'"$PWD"'","mcpServers":[]}}' \
  | ./target/release/ocean-acp
```

You should get back an `initialize` result (`"protocolVersion":1`) and a
`session/new` result with a `sessionId`.

## Permissions and cancellation

ACP turns use the daemon's normal permission policy. With the default gated
configuration, the bridge subscribes to the daemon control stream before turn
submission, forwards each turn-scoped `PermissionRequest` to the editor as
`session/request_permission`, and posts the editor's allow/deny decision to
`POST /v1/permissions/{id}/decision`. Each turn carries a private decision token
so a permission decision is bound to the submitting bridge. If the editor
cancels or the permission round-trip fails, the bridge denies the request so the
daemon waiter is released.

`session/cancel` is also live. The bridge learns the active daemon `turn_id` from
`TurnStarted` and forwards cancellation to
`POST /v1/requests/{turn_id}/cancel` without blocking the ACP dispatch loop.

If the daemon operator explicitly enables global yolo mode, tool gates are
bypassed and no permission request is emitted.

## Limitations / next steps

- **Components**: rendered as Markdown summaries. Rich/interactive rendering has
  no ACP equivalent; the Markdown fallback ensures the content is still shown.
- **Binary prompt content**: text blocks are forwarded directly, but images,
  audio, and embedded resources are currently represented by textual
  placeholders because the daemon turn request does not yet carry ACP binary
  content.
- **Offline session creation**: the local-id fallback keeps `session/new`
  responsive while the daemon is unavailable, but that fallback mapping cannot
  provide cross-restart transcript resume.
- **Authentication**: ACP authentication methods are not implemented; provider
  credentials and daemon access are configured outside the bridge.
