# Ocean runtime operator guide

This guide is for operators running the current `ocean-rs` Rust-native Pi-style coding-agent harness/runtime and its clients. It reflects the repo state as validated from source on 2026-06-06.

## Operating model

`ocean-daemon` is the runtime authority. It owns agent execution, sessions, request state, permission waiters, and event emission. Clients (`ocean-rs`, `ocean-tui`, GUI clients, curl) should treat the daemon as the source of truth and should not run a second agent loop.

`ocean-tui` is the active steering cockpit and Rust-native Tides Mesh MeshFloor over this harness. It is not a passive daemon dashboard: it is where operators steer requests, inspect sessions/events, handle approvals/cancellation, and monitor floor state while the daemon keeps runtime authority.

Current crates in the operator path:

- `ocean-daemon` — local HTTP daemon, default bind `127.0.0.1:4780`.
- `ocean-agent` — in-process prompt/runtime facade used by the daemon.
- `ocean-cli` — command named `ocean-rs`; thin CLI client.
- `ocean-tui` — ratatui steering cockpit plus Rust-native TIDES-MESH MeshFloor/parity view.
- `ocean-core` — shared protocol types for health, prompts, requests, permissions, sessions, and events.

## Startup

From the repo root:

```bash
cargo run -p ocean-daemon
```

The daemon logs a listening line similar to:

```text
ocean-daemon listening addr=127.0.0.1:4780
```

If another daemon is already bound to the same address, startup fails with `Address already in use`.

For a different bind address:

```bash
OCEAN_BIND=127.0.0.1:4781 cargo run -p ocean-daemon
```

Keep `OCEAN_BIND` loopback-only unless the operator has explicitly approved remote exposure and a security layer. CORS is now restricted to a localhost whitelist by default (see [Trust boundary](#trust-boundary-permissions--cors)); the daemon should still be treated as local-only.

## Configuration

### Trust boundary: permissions & CORS

The daemon is a local trust boundary. Two env vars control how strict it is.
**Both default to the safe setting** — you only set them to loosen the daemon.

#### `OCEAN_YOLO` — per-tool permission gating (OCEAN-51)

By default (`OCEAN_YOLO` unset), the product agent-turn path
(`POST /v1/agent/turns` and the `POST /v1/agent/voice` wrapper) **gates every
mutating tool call** through the permission machinery: the daemon emits a
`permission_request` event and the turn blocks until an operator allows or denies
it via `POST /v1/permissions/{id}/decision` (the TUI does this with `Shift-Y` /
`Shift-N`).

**Voice turns and permissions (OCEAN-224).** A spoken interface has no permission
card to click, so a gated voice turn that nothing can answer would silently hang.
`POST /v1/agent/voice` therefore enforces an explicit contract: a voice caller
that can relay an approval mints a per-turn `decision_token` (OCEAN-185), sends it
on the voice turn, and replays the same value on the decision POST — the gate is
then approvable exactly like a text turn. A voice turn with **no** `decision_token`
is accepted only when yolo is effective (every tool auto-approves, so no gate is
ever raised). With no token **and** yolo off, the daemon rejects the turn up front
with `400` and a clear, speakable message ("turn on yolo, or send a
decision_token") instead of letting it stall on an un-answerable prompt.

```bash
# Default: gated. Mutating tools require approval.
cargo run -p ocean-daemon

# Opt in to fire-and-forget for trusted automation. Every tool auto-approved.
OCEAN_YOLO=1 cargo run -p ocean-daemon
```

Accepted truthy values: `1`, `true`, `yes`, `on` (case-insensitive). Falsey:
`0`, `false`, `no`, `off`. Unrecognized/unset falls through to the persisted
default (below).

##### Persisted YOLO default (OCEAN-YOLO)

YOLO is also a **persisted personal default** you set once, so it survives daemon
restarts without needing the env var on every launch (mirrors the persisted
model selection — a tiny `yolo_pref` file in `$OCEAN_CONFIG_DIR`).

```bash
# Read current persisted + effective posture.
curl -s http://127.0.0.1:4780/v1/settings/yolo
# {"ok":true,"persisted":null,"effective":false,"env_override":null}

# Set your personal default ON (persists across restarts).
curl -s -X POST http://127.0.0.1:4780/v1/settings/yolo \
  -H 'content-type: application/json' -d '{"enabled":true}'
# {"ok":true,"persisted":true,"effective":true,"env_override":null}
```

- `persisted` — your saved default (`null` on first run ⇒ off).
- `effective` — what a turn actually uses right now (after env override).
- `env_override` — non-null when `OCEAN_YOLO` is masking your persisted default.

**Precedence (highest wins):** explicit per-request `yolo: true` on a turn →
`OCEAN_YOLO` env (if set to a recognized value) → **persisted setting** → built-in
default (**off**). So you set it once as your default, and env / per-request can
still override for a session (e.g. `OCEAN_YOLO=0` forces gating even if your
persisted default is on).

Default stays **OFF** (gating on) — the persisted setting is opt-in and never
silently flips. It only controls whether tools auto-approve; it does **not**
weaken the permission decision-token binding (OCEAN-185).

> ⚠️ **Behavior change.** Before this fix, `/v1/agent/turns` hardcoded yolo mode,
> so the permission machinery was dead for every shipped surface. Now it is live
> by default. A surface that issues mutating tools (write/edit/bash) but has no
> approval UI will see those turns **stall waiting for a decision**. If a surface
> isn't ready to handle approvals yet, run the daemon with `OCEAN_YOLO=1`
> (trusted/local automation only) until that surface ships an approval flow.
> Read-only turns are unaffected.

#### `OCEAN_ALLOWED_ORIGINS` — CORS whitelist (OCEAN-53)

The daemon previously reflected **any** browser origin (`Access-Control-Allow-Origin: *`),
letting any web page the operator visited drive the local daemon cross-origin.
It now only accepts:

- Loopback web origins on **any** port — `http(s)://localhost`, `http(s)://127.0.0.1`,
  `http(s)://[::1]` (covers `trunk serve` :8080, vite :5173, the surface proxy
  :8790, and the daemon itself).
- `chrome-extension://…` origins — the Ocean side-panel runs from a per-install
  extension id and already declares the daemon in its MV3 `host_permissions`.
- Anything listed in `OCEAN_ALLOWED_ORIGINS` (comma-separated, exact match,
  trailing slash optional) — e.g. a tunnel hostname for phone access.

```bash
# Add a tunnel/host origin for remote (e.g. phone-over-tunnel) access:
OCEAN_ALLOWED_ORIGINS="https://ocean.mytunnel.dev,https://app.example.com" \
  cargo run -p ocean-daemon
```

The surface **proxy** and the **native GPUI** client are server-side / native
HTTP callers and never send a browser `Origin`, so CORS does not gate them — only
direct browser-to-daemon calls (the PWA pointed straight at `:4780`, and the
Chrome extension) are affected.

### Daemon URL for clients

Clients default to `http://127.0.0.1:4780`.

Override with either:

```bash
OCEAN_DAEMON_URL=http://127.0.0.1:4781 cargo run -p ocean-cli -- health
```

or:

```bash
cargo run -p ocean-cli -- --url http://127.0.0.1:4781 health
```

### Model selection

The daemon resolves model selection in this order (`resolve_model_selection` in `crates/ocean-providers/src/lib.rs`):

1. `OCEAN_MODEL` — the explicit choice. The persisted last-used selection is injected here on startup, so a machine that has picked a model before carries it forward as `OCEAN_MODEL`.
2. `OCEAN_DEFAULT_MODEL` — a cold-machine fallback for a box that has never selected anything.
3. **Error.** There is **no** hardcoded default model. With neither variable set (and no persisted selection), the daemon returns `ProviderConfigError::NoModelSelected`:

   > `no model selected — set OCEAN_MODEL or pick one via POST /v1/model (the daemon never defaults to a model for you)`

So a cold start with no model anywhere fails fast rather than silently picking one. Set `OCEAN_MODEL`, or pick a model via `POST /v1/model` (which persists the selection so it flows back in as `OCEAN_MODEL` next time).

Currently mapped model IDs and their aliases (from `resolve_model_selection`; `known_models()` lists the headline set surfaced to clients):

- `deepseek` / `deepseek-chat`
- `deepseek-v4-flash`
- `deepseek-v4-pro` (`deepseek-v4`, `deepseek-pro`, `v4-pro`, and `DeepSeek V4 Pro` normalize here)
- `deepseek-reasoner` / `deepseek-r1`
- `gpt-4o`
- `gpt-4o-mini`
- `gpt-5.5` / `gpt-5-5` (Codex)
- `gpt-5.4` / `gpt-5-4` (Codex)
- `gpt-5.4-mini` / `gpt-5-4-mini` (Codex)
- `gpt-5.3-codex-spark` / `gpt-5-3-codex-spark` (Codex)
- `claude-sonnet-4-6` / `claude-sonnet` / `sonnet`
- `claude-opus-4-7` / `claude-opus` / `opus`
- `minimax` / `minimax-m2` (maps to `MiniMax-M2`)
- `minimax-m2.7` / `minimax-m2-7` (maps to `MiniMax-M2.7`)
- `kimi` / `kimi-k2.6` / `kimi-k2-6`
- `kimi-k2` / `moonshot-v1`
- `gemini` / `gemini-2.0-flash` / `gemini-2-0-flash`
- `fake` / `fake-ok`, `fake-tool`, `fake-surface` (keyless test providers)

Any other model ID uses the OpenAI-compatible provider only when `OCEAN_OPENAI_BASE_URL` is set; otherwise it is rejected as unknown.

Historical note: earlier audits found `deepseek-v4-flash` falling through to the generic OpenAI-compatible path, and stale Pi-era model environment could shadow the operator's intended model. Ocean now keys off `OCEAN_MODEL` only, and maps `deepseek-v4-pro` explicitly so V4 Pro does not silently become Flash.

### API keys

For DeepSeek models, the daemon looks for a credential in this order (`credential_env_names` in `crates/ocean-providers/src/lib.rs`):

1. `OCEAN_DEEPSEEK_API_KEY` (preferred)
2. `DEEPSEEK_API_KEY`
3. The Ocean auth file, resolved as `$OCEAN_CONFIG_DIR/auth.json` → `$XDG_CONFIG_HOME/ocean-rs/auth.json` → `$HOME/.config/ocean-rs/auth.json`. The DeepSeek key is read at JSON pointer `/providers/deepseek/api_key`, falling back to `/deepseek/api_key` then `/deepseek/key`. (There is no `~/.pi/agent/auth.json` — that path is never used.)

The same `OCEAN_`-prefixed-first precedence holds for the other providers:

- OpenAI / OpenAI-compatible: `OCEAN_OPENAI_API_KEY` → `OPENAI_API_KEY`
- Anthropic: `OCEAN_ANTHROPIC_API_KEY` → `ANTHROPIC_API_KEY`
- MiniMax: `OCEAN_MINIMAX_API_KEY` → `MINIMAX_API_KEY`
- Kimi / Moonshot: `OCEAN_MOONSHOT_API_KEY` → `MOONSHOT_API_KEY` → `KIMI_API_KEY`
- Google / Gemini: `OCEAN_GOOGLE_API_KEY` → `GOOGLE_API_KEY` → `GEMINI_API_KEY`
- OpenAI Codex: no env API key — uses the OAuth token from the `openai-codex` block of the auth file

The current health endpoint reports daemon availability and backend name; it does not fully prove provider credential readiness. Run a prompt smoke test before declaring the runtime healthy.

### Session/config directory

`ocean-agent` chooses its config directory from:

1. `OCEAN_CONFIG_DIR`
2. `$XDG_CONFIG_HOME/ocean-rs`
3. `$HOME/.config/ocean-rs`
4. fallback `.ocean-rs`

Sessions are currently JSON-backed. Avoid concurrent prompts into the same session until per-session locking/storage hardening lands.

## Common commands

### CLI help

```bash
cargo run -p ocean-cli -- --help
```

The binary command name is `ocean-rs` and supports:

```text
ocean-rs health
ocean-rs prompt [--yolo] [--max-turns N] <prompt...>
ocean-rs sessions
```

When running from Cargo, include the Cargo separator before CLI args:

```bash
cargo run -p ocean-cli -- health
```

### Health

```bash
cargo run -p ocean-cli -- health
curl http://127.0.0.1:4780/health
```

Expected shape:

```text
ok ocean-daemon backend=<backend-name>
```

HTTP JSON includes:

```json
{"ok":true,"service":"ocean-daemon","version":"...","backend":"..."}
```

### Prompt smoke test

```bash
cargo run -p ocean-cli -- prompt "Reply exactly: OCEAN_OK"
```

A healthy prompt path should return assistant text plus a stderr footer like:

```text
[ocean-rs: ok=true wall=<ms>ms rss=daemon]
```

CLI exit-code behavior (fixed, OCEAN-189): the CLI now exits non-zero when the daemon returns `ok=false` (stdout is still printed first, so output isn't lost). The footer also reports per-turn token usage.

### Health & readiness — which probe to use

The daemon exposes two distinct health endpoints with different meanings.
Both are at the **root**, not under `/v1` — `GET /v1/health` does **not**
exist, so a 404 there means "wrong path", not "daemon down". Verified against
the `health` and `ready` route handlers in `crates/ocean-daemon/src/main.rs`.

**`GET /health` — liveness (process is up and serving HTTP).**

- **Always returns HTTP 200** as long as the process is accepting connections.
  The body's `ok` field is hardcoded `true`; it does **not** reflect provider
  state.
- Body: `{"ok":true,"service":"ocean-daemon","version":"<v>","backend":"<name>"}`.
- Says nothing about whether a provider/credential is configured. A daemon with
  no API key still answers `/health` with 200.

```bash
curl -fsS http://127.0.0.1:4780/health
```

**`GET /ready` — readiness (a provider/model is wired up to execute turns).**

- Calls `runtime.provider_readiness()` and serializes the result. **It also
  always returns HTTP 200** — readiness is carried in the JSON **body**, not the
  HTTP status code. There is no 500 on "not ready".
- Body when ready:
  `{"ok":true,"provider":"<id>","model":"<m>","base_url_host":"<host>","credential_present":true,...}`.
- Body when **not** ready (e.g. no credential for a provider that needs one):
  `{"ok":false,...,"credential_present":false,"error":{"code":"MISSING_CREDENTIAL",...}}` —
  still HTTP 200.
- Therefore an ops probe must inspect the **`ok` field of the body**, not the
  status code, to detect a dead-provider daemon:

```bash
# "ready" only if the body says ok:true — a 200 alone is not enough.
curl -fsS http://127.0.0.1:4780/ready | jq -e '.ok == true' >/dev/null
```

**Which to use where:**

| Check | Endpoint | What "pass" means |
|---|---|---|
| launchd `KeepAlive` (restart-on-death) | `GET /health` | process is alive |
| External readiness / monitoring probe (page on dead provider) | `GET /ready` + assert body `ok:true` | can actually run a turn |

**Tradeoff and recommendation:**

- The launchd job (`ocean-daemon-preview`) should key its `KeepAlive` /
  liveness restart on **`/health`**. Restart only when the process is truly
  gone. Wiring `KeepAlive` to `/ready` would restart-thrash the daemon on a
  transient or operator-pending provider issue (e.g. key not yet set), which
  fixes nothing — the credential is still missing after the restart.
- The cost of `/health`-only KeepAlive is that a daemon with a dead/unconfigured
  provider stays "up" and unrestarted. Cover that gap with a **separate external
  readiness probe** that hits `/ready`, asserts body `ok:true`, and **alerts**
  (does not auto-restart). Give that probe a sane timeout (a few seconds) and
  do not let it drive process restarts.
- Liveness restarts processes; readiness alerts humans. Keep them on different
  endpoints.

Restart the launchd-supervised daemon (a restart drops any in-flight session,
so only restart when intended, not to clear a transient `/ready` blip):

```bash
launchctl kickstart -k gui/$(id -u)/ocean-daemon-preview
```

### Graceful shutdown

The daemon traps `SIGTERM` and `SIGINT` (Ctrl-C) and **drains in-flight work**
before exiting, in two stages, instead of dropping it mid-stream:

1. **Open HTTP connections** are drained by axum's `with_graceful_shutdown`.
2. **Detached turn tasks.** A turn submitted via `POST /v1/requests` runs in a
   background task — the HTTP call returns immediately while the turn keeps
   running. After connections drain, the daemon waits for these registered turn
   tasks to finish before exiting (it does **not** abort them).

The wait in stage 2 is bounded by `OCEAN_SHUTDOWN_GRACE_SECS` (default **20s**;
set `0` to skip waiting). If the budget elapses with turns still running, the
daemon logs a warning and exits anyway, so a stuck turn can't hang shutdown.

Both clients depend on this daemon, so prefer signal-based stops: `launchctl
kickstart -k` (sends `SIGTERM`) or Ctrl-C in the foreground. Avoid `kill -9` /
`SIGKILL`, which still hard-kills the process and aborts whatever turn is in
flight. Note that a long SSE event stream or a running turn will delay exit
until it finishes, disconnects, or the grace budget elapses.

> Known limitation: turns spawned by the room auto-convene path (an `@mention`
> reply) are registered in the request registry but currently do not attach
> their task handle, so stage 2 does not wait on them. The primary
> `POST /v1/requests` turn path is fully drained. (OCEAN-184)

### Sessions

```bash
cargo run -p ocean-cli -- sessions
curl http://127.0.0.1:4780/v1/sessions
```

### TUI steering client

Default coding-agent workspace:

```bash
cargo run -p ocean-tui
```

Default launch now opens the Ocean coding-agent workspace first, with daemon health and system state surfaced as supporting primitives rather than as a standalone monitor view. The primary shell keeps the agent transcript/composer/tool/approval/session surfaces visible while TIDES-MESH rooms are exposed as top-level tabs. Daemon health stays visible but no longer owns the surface.

Workspace operator keys:

- `Tab` — cycle TIDES-MESH rooms
- `F1`..`F7` — jump to `Orchestrator`, `Writers`, `Rev`, `TideDash`, `WorkOps`, `WorldMap`, `PM`
- `Up` / `Down` — change the session target (`new session` or a saved session ID)
- `Enter` — send composer prompt
- `Ctrl-J` — insert newline into the multiline composer
- `Ctrl-U` — clear the composer
- `Ctrl-C` — cancel the latest active request
- `Shift-Y` — allow the newest pending permission request
- `Shift-N` — deny the newest pending permission request
- `F10` or `?` — toggle the inline help surface
- `s` — refresh sessions
- `r` — refresh health/requests/sessions/support state
- `q` / `Esc` — quit

Current honest placeholders in the workspace shell:

- full session transcript inspection still needs `GET /v1/sessions/:id`
- some non-Orchestrator room widgets still render cache/task placeholders instead of embedded native panes
- diff/edit capture is opportunistic from SSE tool output and assistant text, not yet a structured daemon diff feed

### Product direction: permanent multi-room command center

The operator correction after task-26 is explicit:

- Ocean TUI is a permanent Rust-native Ratatui command center, not a daemon monitor with chat attached.
- The default workspace shell stays first-class.
- Daemon monitoring is only one primitive and should eventually live inside an Ops/Systems-style room rather than define the whole product.

Next-phase room model to preserve in docs/planning:

- `PM` — operator communication / PM terminal space
- `Writers Room` — NoteDash-style notes, sources, actions, plus Henry terminal/context lane
- `Tides Mesh` — main mesh command center with Glyph anchor, board/events/inbox/agents primitives, orchestrator control, and live roster/info panel
- `Review Room` — Rev chat plus WorkDash-style PR / Linear / git / diff review primitives
- `TideDash`, `WorkOps/OpsDash`, `WorldMap`, and file-tree contexts as dedicated rooms or panes

Primitive-source rule for future implementation work:

- read `rising-tuis/opsdash.py` for service/ports/cloud health primitives
- read `rising-tuis/workdash.py` for review/git/Linear primitives
- read `rising-tuis/notedash.py` and `rising-tuis/notedash_api.py` for Writers Room notes/source/action primitives
- read `rising-tuis/tidedash.py`, `rising-tuis/world_time_map.py`, and `rising-tuis/file_tree_tui.py` for campaign/world/files primitives
- reimplement the useful primitives natively in Rust/Ratatui rather than treating the Python TUIs as the target surface

Current task-26/task-27 constraint:

- these docs record the roadmap, but they do not authorize new UI/code expansion by themselves
- further implementation should wait for explicit routing/review approval

TIDES-MESH MeshFloor/parity view:

```bash
cargo run -p ocean-tui -- mesh --root . --tab board
cargo run -p ocean-tui -- mesh --root . --tab events
cargo run -p ocean-tui -- mesh --root . --tab inbox
cargo run -p ocean-tui -- mesh --root . --tab agents
```

Mesh view also honors:

- `TIDES_MESH_AGENT`
- `PIMESH_TAB`
- `PIMESH_REFRESH_MS`

Main MeshFloor references:

- [`docs/OCEAN_TUI_TMUX_LAYOUT_MAP.md`](OCEAN_TUI_TMUX_LAYOUT_MAP.md)
- [`docs/OCEAN_TUI_TIDES_MESH_PARITY.md`](OCEAN_TUI_TIDES_MESH_PARITY.md)

## HTTP API quick reference

Full daemon route table, read from `crates/ocean-daemon/src/main.rs` (the
`Router::route()` calls). Grouped by concern:

```text
# Liveness
GET    /                                  root banner
GET    /health                            liveness check
GET    /ready                             readiness (model/credentials wired)

# Agent product API (session-scoped — first-party surfaces)
POST   /v1/agent/turns                    submit a turn { prompt, cwd, session_id, ... }
POST   /v1/agent/voice                    submit a voice turn (transcribed prompt; voice surface)
GET    /v1/agent/events                   SSE stream; ?session_id=<id> scopes to one session
POST   /v1/agent/sessions                 create a session before the first turn
GET    /v1/agent/sessions                 list agent sessions
GET    /v1/agent/sessions/{id}            agent session detail

# Legacy / debug prompt + request API
GET    /v1/events                         global SSE stream (debug/legacy)
POST   /v1/prompt                         synchronous one-shot prompt
GET    /v1/requests                       list async requests
POST   /v1/requests                       enqueue an async request
POST   /v1/requests/{id}/cancel           cancel an in-flight request

# Permissions
GET    /v1/permissions                    list pending permission requests
POST   /v1/permissions/{id}/decision      allow/deny a mutating-tool request

# Rooms — Track-0 projection (RoomSnapshot)
GET    /v1/rooms                          list room projections
GET    /v1/rooms/{room_id}                room projection detail (pm|writers|orch_mesh|review)
POST   /v1/rooms/{room_id}/livekit-token  mint a LiveKit join token for the room (web in-room voice/video)

# Rooms — persistent lifecycle (OCEAN-65; in-memory store)
GET    /v1/rooms/persistent               list persistent rooms
POST   /v1/rooms/persistent               create a room { key, name, trigger_policy? }
GET    /v1/rooms/persistent/{key}         room + transcript
POST   /v1/rooms/persistent/{key}/participants            join { id, display_name, kind? }
DELETE /v1/rooms/persistent/{key}/participants/{id}       leave
POST   /v1/rooms/persistent/{key}/messages                post message { author_id, author_kind?, body }
GET    /v1/rooms/persistent/{key}/transcript              read transcript (?after_seq=N)

# Sessions (legacy view)
GET    /v1/sessions                       list sessions
GET    /v1/sessions/{id}                  session detail / transcript

# Projects (named directory-bound workspaces)
GET    /v1/projects                       list registered projects
POST   /v1/projects                       create a project bound to a directory
GET    /v1/projects/{id}                  project detail
PATCH  /v1/projects/{id}                  update name and/or config (partial)
DELETE /v1/projects/{id}                  delete a project (sessions become project-less)

# Model selection
GET    /v1/model                          current provider/model
POST   /v1/model                          set provider/model
GET    /v1/models                         available models for a client picker

# Surface components
POST   /v1/component/event                surface component interaction event

# Longhouse (council / quorum)
POST   /v1/longhouse/demo                 scripted demo harness (fake events)
POST   /v1/longhouse/convene              convene a real council; events on /v1/agent/events
GET    /v1/longhouse/topics               list longhouse topics
GET    /v1/longhouse/topics/{id}          longhouse topic detail

# Calls (Twilio/LiveKit call-intelligence pipeline — ocean-call)
POST   /v1/calls/demo                     scripted call-pipeline demo (fake transcript/events)
POST   /v1/calls/place                    place an outbound call (SIP bridge → LiveKit room)
POST   /v1/calls/webhook                  Twilio/LiveKit inbound webhook (call status, media)
```

### Synchronous prompt

```bash
curl -sS http://127.0.0.1:4780/v1/prompt \
  -H 'content-type: application/json' \
  -d '{"prompt":"Reply exactly: OCEAN_OK","yolo":false}'
```

### Async request

```bash
curl -sS http://127.0.0.1:4780/v1/requests \
  -H 'content-type: application/json' \
  -d '{"prompt":"Reply exactly: OCEAN_OK","yolo":false}'
```

Then inspect:

```bash
curl -sS http://127.0.0.1:4780/v1/requests
```

Cancel a request:

```bash
curl -sS -X POST http://127.0.0.1:4780/v1/requests/<request-id>/cancel
```

### Permission decision

When the daemon emits a permission request, post a decision to:

```bash
curl -sS -X POST http://127.0.0.1:4780/v1/permissions/<permission-id>/decision \
  -H 'content-type: application/json' \
  -d '{"permission_id":"<permission-id>","decision":"deny","reason":"operator denied"}'
```

Use `{"permission_id":"<permission-id>","decision":"allow"}` only when the operator explicitly approves the requested action. Mutating tools without approval should remain denied unless `--yolo` was intentionally used.

## Logs and events

### Process logs

Foreground daemon logs go to stderr/stdout through `tracing_subscriber`. Use `RUST_LOG` to adjust verbosity:

```bash
RUST_LOG=ocean_daemon=debug cargo run -p ocean-daemon
```

If running under a user service, inspect the relevant journal unit, for example:

```bash
systemctl --user status <ocean-service-name>
journalctl --user -u <ocean-service-name> -f
```

Use the actual unit name from the local install; this repo does not currently define a canonical unit file.

### SSE event stream

Follow daemon events:

```bash
curl -N http://127.0.0.1:4780/v1/events
```

Events are Server-Sent Events carrying an `EventEnvelope` with IDs and optional request/session/permission correlation. Event types include:

- `session_created`
- `user_message`
- `assistant_delta`
- `tool_started`
- `tool_output`
- `tool_ended`
- `permission_request`
- `permission_decision`
- `turn_finished`
- `cancelled`
- `error`

Current caveat: streaming is improving, but some assistant output may still arrive after completion rather than as true token deltas. Treat events as the operational audit surface, not yet a complete durable event store.

## Troubleshooting

### Daemon will not start

- Check for an existing process on `127.0.0.1:4780`.
- Try a different bind with `OCEAN_BIND=127.0.0.1:4781`.
- Confirm the repo builds: `cargo check --all-targets`.

### Health says ok but prompt fails

Health currently means the daemon process and runtime facade initialized. It may not prove provider credentials or model alias correctness.

Run:

```bash
cargo run -p ocean-cli -- health
cargo run -p ocean-cli -- prompt "Reply exactly: OCEAN_OK"
```

If the prompt reports a missing OpenAI key while you expected DeepSeek, check `OCEAN_MODEL`, confirm the deployed binary includes the task-1 model-alias hotfix, and verify the service environment is using the expected model/provider settings.

### CLI reports `ok=false`

Inspect stderr/stdout and the footer. As of OCEAN-189 the CLI exits non-zero on an `ok=false` daemon response (after printing stdout), so scripts can rely on the exit code; the footer also surfaces token usage for cost visibility.

### Permission/tool action stalls

Check:

```bash
curl -sS http://127.0.0.1:4780/v1/requests
curl -N http://127.0.0.1:4780/v1/events
```

Look for a request in `waiting_for_permission` and a matching `permission_request` event. Decide only with explicit operator approval.

### Prompt/session history looks wrong

Sessions are currently JSON-backed and not fully hardened for concurrent writes. Avoid launching overlapping prompts into the same session. Capture the request ID, session ID, command, and logs before handing off.

### TUI is stale or blocked

- Verify the daemon URL with `OCEAN_DAEMON_URL` or `--url`.
- Confirm `/health` and `/v1/events` work via curl.
- For mesh mode, verify the `--root` path points at the repo containing `.pi/messenger` state.

### Security concern

Treat the daemon as local-only. Do not bind to `0.0.0.0`, expose through a tunnel, or run `--yolo`/mutating tools without explicit operator approval and review.

## Safe handoff and review flow

When handing off an Ocean runtime issue, include:

- repo path and branch
- daemon startup method and environment overrides
- exact `OCEAN_BIND`, `OCEAN_DAEMON_URL`, `OCEAN_MODEL`, and relevant key source, redacting secrets
- commands run and outputs
- health JSON
- prompt smoke result, including `ok`, backend, stderr, and exit status if relevant
- request ID, session ID, permission ID, and event excerpts when available
- dirty working tree summary from `git status --short`
- whether any mutating/yolo action was approved by the operator

Recommended routing:

- Runtime/provider/config bugs: BRICK.
- Review, safety framing, merge/release gates: KNOX/Rev.
- TUI/operator UX: PIXEL.
- Research/architecture background: Charlotte.
- Writing/operator docs: Henry.
- Ownership conflicts or production-risk actions: Orchestrator/OWL.

Do not change systemd units, environment files, credentials, daemon binds, or restart production-ish services without explicit operator approval.

## Docs-only validation note

This guide was checked against:

- `README.md`
- `docs/ARCHITECTURE.md`
- `crates/ocean-cli/src/main.rs`
- `crates/ocean-daemon/src/main.rs`
- `crates/ocean-agent/src/lib.rs`
- `crates/ocean-core/src/lib.rs`
- `crates/ocean-tui/src/main.rs`

Commands run for validation:

```bash
git status --short
cargo run -q -p ocean-cli -- --help
cargo run -q -p ocean-tui -- --help
```

No code changes are required by this document. Known gaps called out above should be fixed in code before operators rely on automated health/exit-status checks.
