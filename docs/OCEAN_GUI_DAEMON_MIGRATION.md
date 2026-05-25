# Ocean OS GUI daemon-client migration

Layer: GUI client architecture/docs.

## Goal

Make `/home/ocean-os/ocean-native` a thin native client for the canonical
`ocean-rs` daemon at `/home/smathdaddy/code/rust/ocean-rs`. Ocean OS should
render state, gather user intent, and subscribe to daemon events; it should not
own the agent loop, provider calls, sessions, tool execution, or workspace
mutation policy.

Canonical runtime today:

```text
/home/smathdaddy/code/rust/ocean-rs
GET  /health
POST /v1/prompt
GET  /v1/sessions
```

Current GUI workspace:

```text
/home/ocean-os
```

## Current Ocean OS runtime coupling

`ocean-native` is not thin yet. It links `/home/ocean-os/ocean-runtime` directly
and starts that runtime in-process.

| Path | Current role | Thin-client target |
|---|---|---|
| `/home/ocean-os/Cargo.toml` | Workspace includes `ocean-runtime` and `ocean-native`; `ocean-native` is the default member. | GUI workspace may keep only client/adaptor crates; runtime authority moves to `ocean-rs`. |
| `/home/ocean-os/ocean-native/Cargo.toml` | Direct path dependency: `ocean-runtime = { path = "../ocean-runtime" }`. | Replace with an HTTP/SSE client dependency and shared `ocean-core` protocol types, or a small generated/client crate. |
| `/home/ocean-os/ocean-native/src/main.rs` | Calls `ocean_runtime::RuntimeNode::spawn(current_dir)`, drains runtime events every frame, and forwards composer input with `runtime.send_text`. | Create a daemon client at startup, poll/subscribe to daemon status/events, and send typed prompt/session/cancel/permission requests. |
| `/home/ocean-os/ocean-native/src/state.rs` | Imports `Message`, `Role`, `RuntimeEvent`, `ToolEvent`, `ToolStatus` from `ocean_runtime`; stores raw pending command strings; maps `RuntimeEvent` into GUI state. | Own GUI view models locally and map from `ocean_core::{HealthResponse, PromptResponse, SessionSummary, EventEnvelope}`. |
| `/home/ocean-os/ocean-native/src/ui.rs` | Imports runtime DTOs and emits local commands such as `help`, `ls`, `read <path>`, `check`, and `clear`. | Render daemon health, sessions, transcript, and activity; convert buttons/composer actions into daemon client requests. |
| `/home/ocean-os/ocean-runtime/src/lib.rs` | Owns runtime authority: `RuntimeNode`, `RuntimeCommand`, `RuntimeEvent`, file scan, safe file reads, `./scripts/check.sh`, command parsing, and event emission. | Stop evolving this as an authority. Treat it as a temporary reference adapter, then retire or salvage DTO ideas into `ocean-rs` protocol/client crates. |
| `/home/ocean-os/docs/OCEAN_GUI_DAEMON_MIGRATION.md` | Local migration note already identifies the in-process runtime split. | This ocean-rs document is the canonical cross-repo handoff for daemon-client alignment. |

## Authority boundary to remove

Current GUI path:

```text
ocean-native UI
  -> raw command string
  -> in-process ocean_runtime::RuntimeNode
  -> workspace scan / read / check / local events
  -> ocean-native AppState
```

Target GUI path:

```text
ocean-native UI
  -> typed daemon client request
  -> ocean-rs daemon
  -> ocean-agent / tools / providers / sessions
  -> HTTP response + SSE/EventEnvelope stream
  -> ocean-native view model
```

The GUI may keep native SDL2 + egui rendering, local widget state, command
history, and view-specific filtering. Runtime decisions belong to `ocean-rs`.

## First thin-client slice

Build the GUI migration in the same order as the current daemon surfaces.

### 1. Health

Use `GET /health` to render:

- connected / disconnected
- daemon service and version
- backend/model string, currently `ocean-native-deepseek`
- last successful heartbeat time

This can replace the hard-coded top-bar `online` label before any prompt work.

### 2. Prompt

Use `POST /v1/prompt` with `PromptRequest`:

```json
{"prompt":"user text","request_id":null,"session_id":null,"max_turns":null,"yolo":false}
```

Initial GUI behavior can be simple and blocking: append the returned
`PromptResponse.stdout` or `stderr` to the transcript. Keep command parsing and
tool execution in the daemon, not in `ocean-native`.

### 3. Sessions

Use `GET /v1/sessions` to populate a sessions panel or picker. Preserve the
`SessionSummary.id`/`session_id` value and pass it into future prompt requests
when resuming a conversation.

### 4. Events, cancellation, and permissions

After the daemon exposes the next protocol slice, add:

```text
GET  /v1/events
POST /v1/requests/:id/cancel
POST /v1/permissions/:id/decision
```

Map `EventEnvelope` into GUI transcript/activity view models. The GUI should
surface permission decisions and cancellation controls, but the daemon remains
the authority.

## Migration plan

1. **Freeze the embedded runtime boundary**
   - Do not add new runtime authority to `/home/ocean-os/ocean-runtime`.
   - Keep current GUI behavior working while the daemon client is introduced.

2. **Introduce a GUI client abstraction**
   - Add a small trait or adapter around health, prompt, sessions, and events.
   - Keep an `EmbeddedRuntimeClient` only as a compatibility adapter while
     `DaemonHttpClient` is built.

3. **Wire daemon health first**
   - Query `http://127.0.0.1:4780/health`.
   - Render daemon/backend status in the existing top bar.
   - No provider/tool/session logic in GUI code.

4. **Move composer to daemon prompt**
   - Submit composer text to `/v1/prompt`.
   - Stop sending raw strings to `RuntimeNode::send_text` for normal prompts.
   - Keep old local quick actions behind the compatibility adapter only if
     needed during transition.

5. **Add sessions view**
   - Replace local transcript assumptions with daemon session summaries.
   - Thread `session_id` through prompt requests.

6. **Switch activity rail to daemon events**
   - Once SSE exists in `ocean-rs`, render assistant deltas, tool events,
     permission requests, cancellations, and errors from `EventEnvelope`.

7. **Retire `/home/ocean-os/ocean-runtime` as an authority**
   - Remove the direct path dependency from `ocean-native`.
   - Move any useful DTOs/tests into `ocean-rs` shared protocol/client crates.
   - Keep Ocean OS as a native GUI client only.

## Suggested first implementation PR

A safe first GUI PR after this audit:

- Add a daemon client module to `ocean-native`.
- Fetch `GET /health` on a timer.
- Render service/version/backend plus connection state in the top bar.
- Keep the embedded runtime path temporarily for existing file rail and local
  commands.
- Run `/home/ocean-os/scripts/check.sh`.

This proves the GUI can talk to `ocean-rs` without breaking current behavior.

## Verification from this audit

Commands run from `ocean-rs` before editing this doc:

```bash
systemctl --user status ocean-rs --no-pager
ocean-rs health
cargo check --workspace --all-targets
```

Observed daemon health:

```text
ok ocean-daemon backend=ocean-native-deepseek
```

No `/home/ocean-os` source files were edited for this task.
