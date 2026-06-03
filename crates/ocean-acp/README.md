# ocean-acp

An **Agent Client Protocol (ACP)** bridge that exposes the Ocean daemon to
[Zed](https://zed.dev) (and any other ACP editor — Neovim, Emacs, …) as a
first-class agent.

```
  Zed ──ACP / stdio (newline JSON-RPC)──▶ ocean-acp ──HTTP + SSE──▶ ocean-daemon (:4780)
```

The bridge holds **no agent logic and no sessions of its own**. It translates
ACP requests into calls against the daemon's existing API
(`POST /v1/agent/turns`, `GET /v1/agent/events`) and streams the daemon's
events back as ACP `session/update` notifications. All authority — the agent
loop, tools, sessions, the transcript — stays in the daemon, exactly like the
TUI and ocean-surface clients.

This is the inverse of `ocean-mcp` (which makes Ocean an MCP *client*); here
Ocean is an ACP *agent/server*.

## What works

| ACP method | Mapped to |
|---|---|
| `initialize` | Advertises protocol **v1** + `loadSession` capability |
| `session/new` | Mints an ACP session id, remembers `cwd` |
| `session/prompt` | `POST /v1/agent/turns`, pumps SSE → `session/update`, returns `stopReason` |
| `session/cancel` & others | Acknowledged (no per-turn cancel wired yet) |

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

ACP needs a `sessionId` returned at `session/new`, *before* any turn. But the
daemon mints its real session id **lazily on the first turn** and rejects
client-invented ids on resume. So the bridge keeps a small map:

- `session/new` → return our own ACP id, `daemon_id = None`.
- First `session/prompt` → submit with `session_id: null`; the daemon mints the
  real id (returned in the response); we store it.
- Later prompts → resume against that stored daemon id.

This is verified end-to-end: a second turn correctly recalls context from the
first (the transcript persists daemon-side across turns).

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

## Limitations / next steps

- **Permissions**: v1 relies on the daemon's own permission policy (turns run
  with the daemon's `yolo` path); tool approvals are **not** surfaced to the
  editor yet. The daemon already exposes `GET /v1/permissions` +
  `POST /v1/permissions/{id}/decision`, so forwarding `session/request_permission`
  to Zed is the natural follow-up.
- **Cancel**: `session/cancel` is acknowledged but not wired to a per-turn
  daemon cancel (`POST /v1/requests/{id}/cancel` exists for this).
- **Components**: rendered as Markdown summaries. Rich/interactive rendering has
  no ACP equivalent; the Markdown fallback ensures nothing is lost.
- **`session/load`**: capability is advertised, but cwd repopulation on load
  falls back to the process cwd. Fine for Zed's typical new-thread flow.
