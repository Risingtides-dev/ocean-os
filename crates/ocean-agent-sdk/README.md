# ocean-agent-sdk

**A typed wire vocabulary for the Ocean daemon's HTTP + SSE API — not a runtime.**

This crate does **not** contain an agent loop, provider calls, tool execution, or
sessions. There is no `prompt()` function to call and nothing to embed. It is the
shared set of Rust types (`AgentTurnRequest`, `AgentTurnResponse`, `AgentTurnEvent`,
`ToolResult`, the id newtypes, …) that describe the JSON shapes the **`ocean-daemon`**
speaks over `127.0.0.1:4780`.

You "use" it by depending on it for `serde` (de)serialization while you talk to a
**running daemon** over HTTP. The brain — the agent loop, sessions, permissions,
tools — lives in the daemon (`ocean-runtime` + `ocean-agent`), not here.

```
your Rust integrator                ocean-daemon (:4780)
────────────────────                ────────────────────
AgentTurnRequest  ──POST /v1/agent/turns──▶  runs the turn
AgentTurnResponse ◀────────────────────────  { turn_id, session_id, … }
AgentTurnEvent    ◀──GET  /v1/agent/events── (SSE: text deltas, tool calls, …)
```

If you want to *embed* an agent loop in-process instead of steering a daemon over
HTTP, this is the wrong crate — look at `ocean-runtime` / `ocean-agent`.

---

## The wire flow

The daemon binds to `127.0.0.1:4780` by default (`OCEAN_BIND` to override). All
routes below are on that host.

### 1. (Optional) create a session — `POST /v1/agent/sessions`

You can mint a session up front, or skip this and let the first turn create one
implicitly (submit a turn with `session_id: None`).

- Request: [`AgentSessionCreateRequest`] — `{ workspace_root, project_id?, client_type? }`
- Response: [`AgentSessionCreateResponse`] — `{ session_id, cwd, client_type? }`

### 2. Submit a turn — `POST /v1/agent/turns`

- Request: [`AgentTurnRequest`] — at minimum `{ prompt, cwd }`. Carry a
  `session_id` to continue an existing session; omit it (`None`) to create one.
  Other optional fields: `guidance`, `room_id`, `project_id`, `client_type`,
  `thinking_level`, `model_id`, `images`.
- Response: [`AgentTurnResponse`] — `{ ok, turn_id, session_id, status,
  event_id_prefix, error? }`. `event_id_prefix` is the first 8 chars of the
  turn id, so you can correlate this HTTP response with its SSE events.

This call returns once the turn has been accepted/run; the *streamed* output
(assistant text, tool activity) arrives on the event stream below.

### 3. Stream events — `GET /v1/agent/events?session_id=<id>`

A Server-Sent Events stream. **You must pass `?session_id=<id>`** to receive a
session's events — without it the stream deliberately omits session-bearing
events (an operator firehose is available with `?all=1`). Each SSE `data:` line
is one JSON-encoded [`AgentTurnEvent`]; deserialize each line and match on its
`type` tag.

The stream honors the `Last-Event-ID` header on reconnect: send the last event
id you saw and the daemon replays everything newer before resuming the live
feed, so a dropped connection mid-turn doesn't lose output.

Subscribe to the stream **before** (or concurrently with) submitting the turn so
you don't miss early events.

### 4. (If the daemon gates tools) handle permission requests

See [Permission gating](#permission-gating) below — note it rides a *separate*
legacy event rail and uses `ocean-core` types, **not** the types in this crate.

---

## Key types

| Type | Purpose |
|---|---|
| [`AgentTurnRequest`] | Body you POST to `/v1/agent/turns`. `prompt` + `cwd` required; `session_id` optional (omit to create a session). |
| [`AgentTurnResponse`] | Reply from `/v1/agent/turns`: `turn_id`, `session_id`, `status`, `event_id_prefix`. |
| [`AgentTurnEvent`] | The SSE payload enum (`#[serde(tag = "type")]`). One per `data:` line on `/v1/agent/events`. Variants below. |
| [`ToolResult`] | Result of a completed tool call (`ok`, `output`, `metadata_json?`); carried inside the `tool_call_finished` event. |
| [`AgentSessionCreateRequest`] / [`AgentSessionCreateResponse`] | Explicit `POST /v1/agent/sessions` create. |
| [`AgentSessionId`], [`AgentTurnId`], [`ToolCallId`] | UUID newtypes that thread through every payload. |
| [`AgentTurnStatus`] | `queued` / `running` / `completed` / `failed` / `cancelled`. |

It re-exports [`ThinkingLevel`] from `ocean-protocol` so you can set
`AgentTurnRequest::thinking_level` without depending on the protocol crate
directly. The `surface` module carries the agent-native canvas-patch types
(`SurfacePatch`, `SurfacePatchEnvelope`, `CanvasId`, …) used by the
`SurfacePatch` event.

### `AgentTurnEvent` variants

Tagged by a snake_case `type` field. The ones an integrator cares about most:

| `type` | Meaning |
|---|---|
| `turn_started` | The turn began; carries `turn_id`, `session_id`, and the live `model`. |
| `assistant_text_delta` | Incremental assistant output (`delta`) — append to your buffer. |
| `thinking_delta` | Incremental extended-thinking output; show separately/collapsed. |
| `tool_call_started` | A tool call was dispatched (`call: ToolCall`). |
| `tool_call_chunk` | Streaming output from a running tool call. |
| `tool_call_finished` | A tool call completed (`result: ToolResult`). |
| `turn_finished` | The turn ended; carries final `status`, plus `wall_ms` / token counts when known. |
| `session_created` | A session was implicitly created by a turn with `session_id: null`. |
| `component_render` / `component_unmount` | Agent-driven interactive UI (see `docs/AGENT_RENDER_PROTOCOL.md`). |
| `browser_activity` | A browser tool started/stopped. |
| `surface_patch` | Validated canvas patches (GPUI Masterbuild); see the `surface` module. |
| `extension` | Catch-all for extension events (e.g. Longhouse council events). |

Clients should ignore unrecognised `type` values to stay forward-compatible.

---

## A worked example

> **Illustrative.** This crate ships only the types (it has no HTTP client
> dependency), so the snippet below assumes you add `reqwest`, `tokio`,
> `serde_json`, and an SSE reader on your side. The routes, field names, and
> types are real and match the daemon; treat the transport scaffolding as a
> sketch you adapt to your HTTP/SSE stack of choice.

```rust,ignore
use ocean_agent_sdk::{AgentTurnEvent, AgentTurnRequest, AgentTurnResponse};

const BASE: &str = "http://127.0.0.1:4780";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let http = reqwest::Client::new();

    // 1. Submit a turn. Omit `session_id` to let the daemon create a session;
    //    the response (and a `session_created` event) tells you the new id.
    let req = AgentTurnRequest {
        session_id: None,
        prompt: "list the files in src/".into(),
        cwd: "/path/to/your/repo".into(),
        guidance: None,
        room_id: None,
        project_id: None,
        client_type: Some("my-integration".into()),
        thinking_level: None,
        model_id: None,
        images: None,
    };
    let resp: AgentTurnResponse = http
        .post(format!("{BASE}/v1/agent/turns"))
        .json(&req)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let session_id = resp.session_id;
    println!("turn {} in session {}", resp.turn_id, session_id);

    // 2. Stream this session's events. MUST pass ?session_id=<id> — without it
    //    the stream omits session-bearing events by design.
    let mut stream = http
        .get(format!("{BASE}/v1/agent/events?session_id={session_id}"))
        .header("Accept", "text/event-stream")
        .send()
        .await?
        .bytes_stream();

    // Pseudo-SSE: in practice use an SSE parser. Each `data:` line is one
    // JSON-encoded AgentTurnEvent.
    while let Some(line) = next_sse_data_line(&mut stream).await? {
        let event: AgentTurnEvent = serde_json::from_str(&line)?;
        match event {
            AgentTurnEvent::AssistantTextDelta { delta, .. } => print!("{delta}"),
            AgentTurnEvent::ToolCallStarted { call, .. } => {
                eprintln!("\n[tool] {} {}", call.name, call.args_json);
            }
            AgentTurnEvent::TurnFinished { status, .. } => {
                eprintln!("\n[turn finished: {status:?}]");
                break;
            }
            // Ignore unrecognised / uninteresting variants for forward-compat.
            _ => {}
        }
    }
    Ok(())
}
```

For a **real, compiling** end-to-end integration — including the SSE reader and
the permission bridge — read `crates/ocean-cli/src/main.rs`. (Note: the CLI
currently drives the *legacy* `/v1/prompt` + `/v1/events` rail using `ocean-core`
types; the `/v1/agent/*` rail described here is the product-level equivalent the
TUI, ACP bridge, and ocean-surface use.)

---

## Permission gating

On a daemon that gates tools (the default — not running under `--yolo` /
auto-approve), a mutating tool call **blocks** and the daemon emits a
**`PermissionRequest`**. The blocked turn does not proceed until a decision is
POSTed back.

Two things to know that this crate's types do **not** cover, because permission
gating predates the `/v1/agent/*` rail and lives in `ocean-core`:

- The `PermissionRequest` arrives on the **legacy `OceanEvent` stream**
  (`GET /v1/events`), as `OceanEvent::PermissionRequest { tool, reason, args }`,
  with the `permission_id` on the event envelope — **not** as an `AgentTurnEvent`
  on `/v1/agent/events`. To answer permission prompts you subscribe to
  `/v1/events` in addition to `/v1/agent/events`.
- You answer it with `POST /v1/permissions/{id}/decision`, body
  [`ocean_core::PermissionDecisionRequest`] `{ permission_id, decision }`, where
  `decision` is one of `allow`, `allow_session` (remember the choice for this
  run), or `deny { reason? }`.

`crates/ocean-cli/src/main.rs` (`permission_bridge` + `post_decision`) is the
reference implementation: it subscribes to `/v1/events` before submitting the
turn, matches `PermissionRequest`s, and POSTs the decision so the blocked turn
unblocks. A non-interactive caller should default to **deny** on prompts it
can't answer.

---

## Crate facts

- **Name / version:** `ocean-agent-sdk` `0.1.0`.
- **Depends on** `ocean-protocol` (for the re-exported `ThinkingLevel`),
  `serde` / `serde_json`, `chrono`, `uuid`. It has **no** HTTP client and **no**
  runtime dependency — it is pure data types.
- **Sibling crates:** `ocean-core` (lower-level protocol types incl. the
  permission types above), `ocean-runtime` / `ocean-agent` (the actual agent
  loop + sessions), `ocean-daemon` (the HTTP service you talk to).

[`AgentTurnRequest`]: src/lib.rs
[`AgentTurnResponse`]: src/lib.rs
[`AgentTurnEvent`]: src/lib.rs
[`ToolResult`]: src/lib.rs
[`AgentSessionCreateRequest`]: src/lib.rs
[`AgentSessionCreateResponse`]: src/lib.rs
[`AgentSessionId`]: src/lib.rs
[`AgentTurnId`]: src/lib.rs
[`ToolCallId`]: src/lib.rs
[`AgentTurnStatus`]: src/lib.rs
[`ThinkingLevel`]: https://docs.rs/ocean-protocol
[`ocean_core::PermissionDecisionRequest`]: ../ocean-core/src/lib.rs
