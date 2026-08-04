# Ocean runtime operator guide

Status: extended configuration/API/troubleshooting reference. Start with the
current concise runbook in [`OPERATIONS.md`](OPERATIONS.md). This file contains
more detailed subsystem guidance accumulated over time; exact routes, fields,
model aliases, and deployment behavior remain subordinate to current typed
source, tracked scripts, and tests.

This guide is for operators running the `ocean-rs` Rust-native agent runtime and
its clients.

## Operating model

`ocean-daemon` is the runtime authority. It owns agent execution, sessions, request state, permission waiters, and event emission. Clients (`ocean-rs`, `ocean-tui`, GUI clients, curl) should treat the daemon as the source of truth and should not run a second agent loop.

`ocean-tui` is the active terminal workbench over this harness. Operators steer turns, inspect native sessions, edit/browse project files, use the graph and terminal dock, and handle approvals while the daemon keeps runtime authority.

Current crates in the operator path:

- `ocean-daemon` — local HTTP daemon, default bind `127.0.0.1:4780`.
- `ocean-agent` — in-process prompt/runtime facade used by the daemon.
- `ocean-cli` — command named `ocean-rs`; thin CLI client.
- `ocean-tui` — ratatui chat/session/files/editor/graph/terminal workbench.
- `ocean-core` — shared protocol types for health, prompts, requests, permissions, sessions, and events.

## Startup

Build in the repository, then start the absolute daemon binary from a neutral
working directory. Do not bypass the repository-cwd guard: an unbound fallback
turn must never inherit this checkout accidentally.

```bash
cargo build -p ocean-daemon
export OCEAN_DAEMON_BIN="$(pwd)/target/debug/ocean-daemon"
(cd "$HOME" && "$OCEAN_DAEMON_BIN")
```

The daemon logs a listening line similar to:

```text
ocean-daemon listening addr=127.0.0.1:4780
```

If another daemon is already bound to the same address, startup fails with `Address already in use`.

For a different bind address:

```bash
(cd "$HOME" && OCEAN_BIND=127.0.0.1:4781 "$OCEAN_DAEMON_BIN")
```

Keep `OCEAN_BIND` loopback-only unless the operator has explicitly approved remote exposure and a security layer. CORS is now restricted to a localhost whitelist by default (see [Trust boundary](#trust-boundary-permissions--cors)); the daemon should still be treated as local-only.

## Configuration

### Trust boundary: permissions & CORS

The daemon is a local trust boundary. Two env vars control how strict it is.
**Both default to the safe setting** — you only set them to loosen the daemon.

#### Permission approval modes

The daemon owns one persisted global approval mode, captured when each turn
starts. In the TUI, run `/permissions` and choose:

- **Manually approve** (`manual`) — pause for every known tool action.
- **Automatically approve** (`automatic`, the default) — run safe/read-only
  tools and pause for tools the runtime classifies as mutating or side-effecting.
- **Skip all approvals** (`skip_all`) — never pause, even for unsafe actions.

A mode is captured when a turn starts. If the TUI is already waiting on a
permission and the daemon confirms `skip_all`, the TUI authorizes only the
already-active request and releases its pending and later same-turn prompts
through the normal token-bound decision endpoint. That bridge authorization is
cleared when the request finishes and never applies to a new turn.

The call active lane is stricter and independent of that wrapper: every
`client_type: "call-voice"` turn uses the Voice harness profile, forces
`yolo: false`, and disables the complete tool registry. `OCEAN_YOLO=1` does not
loosen this posture, so a call answer cannot execute tools or raise a permission
request.

```bash
# Read the saved and effective mode.
curl -s http://127.0.0.1:4780/v1/settings/permissions
# {"ok":true,"persisted":null,"effective":"automatic"}

# Persist a choice for subsequent turns.
curl -s -X POST http://127.0.0.1:4780/v1/settings/permissions \
  -H 'content-type: application/json' -d '{"mode":"manual"}'
```

`persisted` is the saved choice (`null` before the first selection), `effective`
is what a new turn will use, and `env_override` appears when `OCEAN_YOLO` masks
the saved mode. Existing `yolo_pref=true/false` files migrate on read to
`skip_all`/`automatic`. The legacy `GET/POST /v1/settings/yolo` boolean adapter
remains available and writes the same underlying choice.

`OCEAN_YOLO=1` forces `skip_all`. `OCEAN_YOLO=0` prohibits `skip_all`, but keeps a
saved distinction between `manual` and `automatic`; a saved `skip_all` becomes
effectively `automatic`. The legacy `PromptRequest.yolo` wire flag is inert and
cannot escalate daemon authority.

Permission decisions remain bound to the submitting turn's `decision_token`
(OCEAN-185). A voice turn without a token is accepted only in effective
`skip_all` mode; otherwise the daemon rejects it up front rather than hanging on
a prompt the spoken interface cannot answer.

#### `OCEAN_ALLOWED_ORIGINS` — CORS whitelist (OCEAN-53)

The daemon previously reflected **any** browser origin (`Access-Control-Allow-Origin: *`),
letting any web page the operator visited drive the local daemon cross-origin.
It now only accepts:

- Loopback web origins on **any** port — `http(s)://localhost`, `http(s)://127.0.0.1`,
  `http(s)://[::1]` (covers `trunk serve` :8080, vite :5173, the surface proxy
  :8790, and the daemon itself).
- `chrome-extension://…` origins — the Ocean side-panel runs from a per-install extension id and already declares the daemon in its MV3 `host_permissions`.
- Tauri webview origins `tauri://localhost` and `https://tauri.localhost`.
- Anything listed in `OCEAN_ALLOWED_ORIGINS` (comma-separated, exact match,
  trailing slash optional) — e.g. a tunnel hostname for phone access.

```bash
# Add a tunnel/host origin for remote (e.g. phone-over-tunnel) access:
(cd "$HOME" && OCEAN_ALLOWED_ORIGINS="https://ocean.mytunnel.dev,https://app.example.com" \
  "$OCEAN_DAEMON_BIN")
```

The surface proxy and other native HTTP callers do not send a browser `Origin`. Direct browser, extension, and Tauri webview requests are origin-checked against the policy above.

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

Do not copy the model inventory into operational docs: it changes independently of this guide. `ocean-providers::known_models` and `resolve_model_selection` are authoritative, and clients should use the daemon's model catalog/readiness routes. Unknown model IDs use the OpenAI-compatible provider only when `OCEAN_OPENAI_BASE_URL` is set; otherwise they are rejected.

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

Sessions are currently JSON-backed. Same-session load, run, and save is serialized by a per-session execution lock; different sessions can run concurrently.

Two additional SQLite databases live alongside sessions and the config dir:

- `rooms.db` — persistent room store (`SqliteRoomStore`). Survives daemon restarts; override the full path with `OCEAN_DB_PATH`.
- `titles.db` — Longhouse title/escrow registry (firekeeper titles, recall tallies). Survives daemon restarts; override the full path with `OCEAN_TITLES_DB_PATH`.

### Federated-room Bedrock bridge

Set `OCEAN_FEDERATION_URL` to the Bedrock **origin only** (for example
`https://bedrock.example.com` or trusted-loopback diagnostics such as
`http://127.0.0.1:8787`). The daemon rejects userinfo, paths, query strings,
fragments, non-HTTPS remote origins, and redirects. Each room bearer remains
in owner-only `rooms.db`; requests use the Authorization header and never a
query token. Missing or invalid configuration moves every credentialed,
non-revoked room to `recovering` instead of leaving stale `live` chrome.

Set daemon-only `OCEAN_FEDERATION_OWNER_TOKEN` when this daemon may bootstrap
an existing Local room as its Bedrock owner and mint invites. The value is read
once at startup, never accepted from a surface request, and after registration
is retained only in owner-only `rooms.db`. Missing owner token disables Local
bootstrap only; existing credentialed rooms and invite redemption still work.

At startup, the AppState-owned federation supervisor enumerates durable room
credentials and starts one cancellable task tree per room. The receiver
reconnects the Bedrock room SSE from the persisted cursor, commits the roster
before the first event, and treats ordered SSE as the **only** confirmation
rail: a ledger POST `201` does not append a transcript row or remove outbox
state. The sender scans durable Pending rows periodically (Notify only reduces
latency), posts one row at a time, and suppresses immediate reposts while the
same connection awaits SSE confirmation. Restart safely retries the exact
producer tuple. Existing rooms start before a background recovery worker
replays every durable pending-redemption triple once per boot with at most four
network exchanges in flight; terminal redeem 403 or self-join 401/403 removes
the triple, while retryable failures retain it.

Operator-visible access states are `connecting`, `recovering`, `live`, and
`revoked` for credentialed rooms (`local` for unfederated rooms). Presence
follows the authenticated SSE lease: during healthy catch-up, the local human
and locally-bound owned agents remain Live even while the state label is
`recovering`; disconnect/resync/auth failure downgrades all projected members
to Unavailable in the same access commit/wake. A room-level revoke closes new
sender admission, fails local Pending rows, persists `revoked` last, emits one
access wake, and stops retries. A Local message keeps the existing immediate
201 transcript/trigger path. Once a room credential is installed, human posts
and bound-agent replies return/enter a 202 Pending outbox only; browser author
claims are ignored, and only ordered SSE appends the confirmed transcript.
Confirmed mentions dispatch only under local policy with positive current User
evidence and a current safe locally-owned Agent roster member plus private
member→folder-agent binding. Claims commit before
nonblocking local dispatch, so replay is at-most-once; federated auto-convene
and failure rows never create divergent unconfirmed transcript entries.

The daemon exposes owner invite creation, restart-safe idempotent redemption,
and safe local-agent registration under `/v1/rooms/persistent`. Invite success
is the only response that carries an invite code. Bearers cross only in
Bedrock-bound daemon requests (Authorization or the durable redeem exchange),
and deterministic registration keys cross only in the Bedrock agent batch.
Neither enters a surface request, projection, transcript, log, or error; local
paths, tools, provider credentials, and permission posture never cross Bedrock.

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
- Body: `{"ok":true,"service":"ocean-daemon","version":"<v>","backend":"<name>","persist_failures_total":<n>}`.
  `persist_failures_total` mirrors `ocean_persist_failures_total` in `GET /metrics` — it is the count of dropped call-transcript writes since daemon start. `0` is healthy; a non-zero value means the SQLite room store is silently losing writes.
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

- The launchd job (`dev.risingtides.ocean-daemon`) should key its `KeepAlive` /
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
launchctl kickstart -k gui/$(id -u)/dev.risingtides.ocean-daemon
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

#### `OCEAN_MAX_CONCURRENT_TURNS` — concurrent-turn ceiling (OCEAN-304)

The daemon limits how many agent turns can run simultaneously via a bounded semaphore. When the pool is full, `POST /v1/agent/turns` returns HTTP 429. The lower-level `POST /v1/requests` compatibility route returns its normal HTTP 200 envelope with `ok:false`; neither route queues the turn. Default: **24**. Set `OCEAN_MAX_CONCURRENT_TURNS` to a positive integer to override; a `0` or non-numeric value falls back to the default rather than shutting off intake.

```bash
(cd "$HOME" && OCEAN_MAX_CONCURRENT_TURNS=8 "$OCEAN_DAEMON_BIN")
```

This ceiling is high enough that normal multi-room / multi-client use never trips it; it exists to bound burst or runaway-loop fan-out before it exhausts provider quota or host memory.

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

Default launch opens the sole Ocean terminal workbench. The left rail resumes
UUID-backed daemon sessions natively; it never launches another TUI inside the
terminal dock. The former Track-0 room cockpit, `--legacy` escape hatch, and
`ocean-tui mesh` parity surface have been removed.

Primary controls:

- `Enter` — submit the chat composer or resume the selected session
- `Ctrl-J` — insert a composer newline
- `Ctrl-R` — search prompt history
- `Ctrl-O` — expand/collapse tool drawers
- `Ctrl-Y` / `Ctrl-N` — allow/deny a pending permission request
- `Tab` — complete an active slash/file picker, otherwise cycle focus
- bottom navigation buttons — sessions, chat, editor, graph, terminal, files
- `/new` — start a fresh daemon session in the active project
- `/models`, `/thinking`, `/login`, `/settings` — runtime controls

Bare `/login` separates **Agent models** from **Voice models**. Agent rows keep
their existing OAuth/API-key behavior. Voice keys are entered in the same masked
popup but persist independently: xAI STT/TTS uses the `xai` auth block (or
`XAI_API_KEY`), while OpenAI Realtime uses `openai-realtime` (or
`OCEAN_OPENAI_REALTIME_API_KEY` / `OPENAI_REALTIME_API_KEY`). Realtime voice
does not inherit the agent `openai` API key or Codex OAuth. Embedding choices
are not advertised here until Ocean has a live typed embedding consumer; shared
semantic search remains owned by ocean-bedrock.

Launch targeting:

```bash
cargo run -p ocean-tui -- --project /path/to/repo
cargo run -p ocean-tui -- --project /path/to/repo --session <uuid-or-prefix>
```


`--session` resolves persisted Ocean sessions and binds their transcript/event
stream directly in the workbench. Without it, startup opens the centered chooser
for a new session, explicit resume, blank editor, or graph; it never auto-resumes.
`/new` explicitly starts clean after entering the workbench.


## HTTP API quick reference

Full daemon route table, read from `crates/ocean-daemon/src/main.rs` (the
`Router::route()` calls). The daemon's route-contract test keeps this table and
`GET /` discovery in exact parity with the assembled router:

```bash
cargo test -p ocean-daemon router_contract -- --nocapture
```

Grouped by concern:

```text
# Liveness / observability
GET    /                                  root banner (JSON route list)
GET    /health                            liveness check
GET    /ready                             readiness (model/credentials wired)
GET    /metrics                           Prometheus text (v0.0.4); Content-Type: text/plain; version=0.0.4

# Agent product API (session-scoped — first-party surfaces)
POST   /v1/agent/turns                    submit a turn { prompt, cwd, session_id, ... }
POST   /v1/agent/voice                    submit a voice turn (transcribed prompt; voice surface)
GET    /v1/agent/events                   SSE stream; ?session_id=<id> scopes to one session
POST   /v1/agent/canvas/fulfill           Slack canvas bridge posts a fulfilled read/list/create result {session_id, op, result}
GET    /v1/agent/canvas/fulfill           query a stored canvas fulfillment ?session_id=&canvas_id=
POST   /v1/agent/sessions                 create a session before the first turn
GET    /v1/agent/sessions                 list agent sessions
GET    /v1/agent/sessions/{id}            agent session detail
GET    /v1/agent/sessions/{id}/config     session config: model/provider, permission projection, model_source
PATCH  /v1/agent/sessions/{id}/config     repin the session's model { model } (catalog-validated; emits session_config_changed)
GET    /v1/agent/history/search            bounded persisted display-transcript search (?q=<query>&limit=<1..50>, default 20)
POST   /v1/agent/sessions/{id}/messages   append an out-of-turn user message to an existing session

# Voice surface support
POST   /v1/voice/realtime/client-secret   mint an ephemeral OpenAI Realtime client secret
POST   /v1/voice/stt                      transcribe audio through xAI speech-to-text
POST   /v1/voice/tts                      synthesize speech through xAI text-to-speech

# Ocean Buddy (typed attachment ingress; native iPhone/watchOS voice reuses the Realtime secret route above)
POST   /v1/ocean-buddy/events              accept a mocked attached lifecycle event and return a Watch result card

The Realtime secret request defaults to conversation when `purpose` is omitted.
A conversation bound by daemon-owned session `workspace_root`/`cwd` to a registered project or
live linked worktree receives `render_component`, `write_handoff`, and bounded
read-only `list_workspace` / `read_workspace_file` tools. The normalized secret
response includes that canonical `workspace_root` so Surface can freeze tool
fulfillment to the exact daemon-authorized root. Session-less, unknown-session,
and project-less conversations retain render + handoff only. Conversation roots
are never accepted from the browser or model.

The additive Voice Planner request is:

```json
{"purpose":"planner","planner_context":{"project_id":"<uuid>","workspace_root":"/canonical/project-or-worktree"}}
```

The daemon resolves the registered project, canonicalizes the main root and live
Git worktrees, and advertises bounded workspace reads plus one closed
`propose_handoff` tool. The proposal tool is non-executing. Only a human click in
Surface crosses the mutation boundary: both flows create through
`POST /v1/agent/sessions`; Create draft then appends `kind: "planner_handoff"`
through the existing messages route, while Create & start submits exactly one
normal `POST /v1/agent/turns` with a fresh decision token. Planner minting itself
creates no session, message, turn, or filesystem change.

# Legacy / debug prompt + request API
GET    /v1/events                         global SSE stream (debug/legacy)
POST   /v1/prompt                         synchronous one-shot prompt

# Observatory (read-only, scoped observer token required)
GET    /v1/observatory/snapshot           consistent projection at a watermark cursor (nodes, edges, attention, instance ids)
GET    /v1/observatory/events             SSE live tail with durable resume (Last-Event-ID or ?after=), reset/gap frames, 3s keepalive
GET    /v1/observatory/replay             ascending bounded JSON event pages (?after=<cursor>&through=&limit=&filter=), 410 on retention-crossed ranges

# Extensions (read-only Phase 1 state; never executes package code)
GET    /v1/extensions/{id}/inspect        installed/trusted/enabled projection (?project_id=<registered-uuid>)
GET    /v1/extensions/{id}/doctor         static state/digest/manifest/trust diagnostics (?project_id=<registered-uuid>)
GET    /v1/requests                       list async requests
POST   /v1/requests                       enqueue an async request
POST   /v1/requests/{id}/cancel           cancel an in-flight request

# Permissions
GET    /v1/permissions                    list pending permission requests
POST   /v1/permissions/{id}/decision      allow/deny a mutating-tool request

# Rooms — persistent lifecycle (SQLite-backed; survives restarts)
GET    /v1/rooms/persistent               list persistent rooms
POST   /v1/rooms/persistent               create a room { key, name, trigger_policy? }
GET    /v1/rooms/persistent/{key}         room + transcript
POST   /v1/rooms/persistent/{key}/participants            join { id, display_name, kind? }
DELETE /v1/rooms/persistent/{key}/participants/{participant_id}  leave
POST   /v1/rooms/persistent/{key}/messages                post message { author_id, author_kind?, body }
GET    /v1/rooms/persistent/{key}/transcript              read transcript (?after_seq=N&limit=M)
POST   /v1/rooms/persistent/{key}/artifacts               record what the room produced { id, kind: task|decision|note, title, body?, author_id }; 201 { artifact }. Author must be on the roster (403). Every create writes a System transcript line in the SAME transaction, so an artifact can never exist that the room's history does not explain.
GET    /v1/rooms/persistent/{key}/artifacts               list this room's artifacts, most recently changed first
POST   /v1/rooms/persistent/{key}/artifacts/{artifact_id}/amend   rewrite in place under compare-and-swap { expected_version, title?, body?, state?, author_id }; 200 { artifact }, 409 { code: artifact_version_conflict, expected_version, actual_version } when the artifact moved on — re-read and retry, never a silent merge; 404 unknown artifact
GET    /v1/rooms/persistent/{key}/snapshot                hydrate: room+participants+transcript+last_seq+next_seq+has_more (?after_seq=N&limit=M)
GET    /v1/rooms/persistent/{key}/events                  SSE: initial full room_access projection (no id) + id-bearing room_message frames via ?after_seq=N / Last-Event-ID replay, then post-commit access-update + message tail; open non-call rooms only
POST   /v1/rooms/persistent/{key}/outbox/retry            retry a locally-authored federated event awaiting Bedrock confirmation { client_event_id }; 202 on success, 403 revoked, 404 unknown room/item, 409 pending/local, 400 malformed body, 500 sanitized store error
POST   /v1/rooms/persistent/{key}/invites                 bootstrap owner if Local, then mint invite { recipient_name?, ttl_minutes? }; raw InviteResponse 201
POST   /v1/rooms/persistent/invites/redeem                restart-safe redeem/self-join { code }; raw RoomAccessProjection 200
POST   /v1/rooms/persistent/{key}/members/agents          register safe local agent descriptors { agent_names }; raw RoomAccessProjection 200

# Room media — retained independently from the retired projection API
POST   /v1/rooms/{room_id}/livekit-token                  mint a LiveKit join token for web in-room voice/video

# Sessions (legacy view)
GET    /v1/sessions                       list sessions
GET    /v1/sessions/{id}                  session detail / transcript
POST   /v1/sessions/{id}/compact          replace transcript and return visible snapshot + SSE fence (404 unknown, 409 busy, 429 capacity)
GET    /v1/sessions/{id}/sync             refresh-only visible snapshot + SSE replay fence

# Folder-as-agent definitions
GET    /v1/agents                         list discoverable agent folders
GET    /v1/agents/{name}                  resolve one agent folder

# Projects (named directory-bound workspaces)
GET    /v1/projects                       list registered projects
POST   /v1/projects                       create a project bound to a directory
GET    /v1/projects/{id}                  project detail
PATCH  /v1/projects/{id}                  update name and/or config (partial)
DELETE /v1/projects/{id}                  delete a project (sessions become project-less)

# GitHub repository projection (public repositories only; read-only, no PAT or Authorization header)
GET    /v1/repo/github/{project_id}/pulls                         list pull requests (?state=open|closed|all&page=1&per_page=10; max 25)
GET    /v1/repo/github/{project_id}/pulls/{number}                pull request detail
GET    /v1/repo/github/{project_id}/head-sha/{sha}/checks         checks for one admitted full 40-hex head SHA
GET    /v1/repo/github/{project_id}/pulls/{number}/reviews        list pull reviews (?page=1&per_page=10; max 25)
GET    /v1/repo/github/{project_id}/commits                       list commits (?sha=main&page=1&per_page=10; max 25)

These routes resolve only a registered project's workspace-root `origin` and
accept only exact public GitHub remote forms. They never accept or forward a PAT,
never send an `Authorization` header, and expose no aggregate or write route.

# Filesystem and browser surface support
GET    /v1/fs/dirs                        list home-sandboxed directories
GET    /v1/fs/file                        read a home-sandboxed file
GET    /v1/browser/screencast             SSE stream of the agent browser's screencast
POST   /v1/browser/input                  forward pointer/keyboard input to the agent browser

# Model selection
GET    /v1/model                          current provider/model
POST   /v1/model                          set provider/model
GET    /v1/models                         available models for a client picker

# Memory and workspace intelligence
GET    /v1/memory                         list retained long-term memories
GET    /v1/lsp                            list language-server readiness for a workspace

# Settings
GET    /v1/settings/yolo                  legacy boolean approval posture
POST   /v1/settings/yolo                  legacy adapter { enabled: bool }
GET    /v1/settings/permissions           read saved + effective approval mode
POST   /v1/settings/permissions           set approval mode { mode: manual|automatic|skip_all }

# Surface components
POST   /v1/component/event                surface component interaction event

# Longhouse (council / quorum / governance)
POST   /v1/longhouse/demo                 scripted demo harness (fake events)
POST   /v1/longhouse/convene              convene a real council; events on /v1/agent/events
POST   /v1/council/convene                alias of /v1/longhouse/convene (same handler)
POST   /v1/longhouse/prepare              read-only pre-turn prep: compact skill briefs (advisory, no gate bypass)
POST   /v1/longhouse/inspect              explain the exact prep ranking with path-redacted compact matches/counts (advisory, no raw-prompt/body/path echo)
POST   /v1/skills/query                   skill-librarian prefilter: rank skills for an intent (advisory, read-only)
POST   /v1/skills/fetch                   skill-librarian fetch: one skill's full body by id (advisory, read-only)
POST   /v1/subagents/spec                 compatibility: assemble advisory spec only (no spawn; extension migration pending)
GET    /v1/longhouse/topics               list longhouse topics
GET    /v1/longhouse/topics/{topic_id}    longhouse topic detail
POST   /v1/longhouse/claim                ratify a converged outcome against the title registry { title_id, agent_id, token, decision }
POST   /v1/longhouse/board                append a note/evidence mark to a topic's durable board { topic_id, author, kind?, summary }
POST   /v1/longhouse/revoke               operator hard-pull of a live title { title_id, reason? }
POST   /v1/longhouse/recall               cast a no-confidence vote in a seated firekeeper { topic_id, firekeeper_id, voter_id, threshold? }
POST   /v1/longhouse/breach               report a policy breach, accruing a graduated strike { title_id, detail? }
POST   /v1/workflows/prepare              select advisory workflow briefs for a turn

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

### Health/readiness metrics — GET /metrics

`GET /metrics` exposes the daemon's observability surface in Prometheus text exposition format (v0.0.4). Read-only — the scrape never touches the hot agent-turn path or takes a lock. Four metric families:

| Metric | Type | Description |
|---|---|---|
| `ocean_turns_total{outcome="ok"\|"error"}` | counter | Finished turns by outcome |
| `ocean_turns_in_flight` | gauge | Turns currently executing |
| `ocean_turn_duration_seconds` | histogram | Turn wall-clock latency (cumulative buckets + `_sum`/`_count`) |
| `ocean_persist_failures_total` | counter | Dropped call-transcript writes (mirrors `GET /health` `persist_failures_total`); `0` is healthy |

Content-Type is `text/plain; version=0.0.4; charset=utf-8`. Sample scrape:

```bash
curl -s http://127.0.0.1:4780/metrics
# ocean_turns_total{outcome="ok"} 22
# ocean_turns_total{outcome="error"} 3
# ocean_turns_in_flight 0
# ocean_turn_duration_seconds_bucket{le="5"} 13
# ...
# ocean_persist_failures_total 0
```

Point Prometheus or any compatible scraper at `:4780/metrics`. A non-zero `ocean_persist_failures_total` means the SQLite room store is silently losing transcript writes — investigate immediately.

### Longhouse governance routes

The five Longhouse governance routes implement the firekeeper title lifecycle. All accept JSON, return `{ ok, … }`, and go through the daemon's own unforgeable `Revoker` key (held on `AppState`, never emitted on the wire).

**POST /v1/longhouse/claim** — ratify a converged outcome (OCEAN-272).

```json
{ "title_id": "<uuid>", "agent_id": "<uuid>", "token": "<secret>", "decision": "<uuid>" }
```

Status: `200` on a ratified claim; `403` for a forged/revoked title (`ForgedFirekeeper`); `409` for premature (`NotConverged`) or wrong-proposal (`WrongDecision`) claim; `400` on a malformed UUID. On success the title is released and the topic's validator escrow is freed.

**POST /v1/longhouse/board** — append a note/evidence mark to a tracked topic's durable board (OCEAN-272). `kind` is `note` (default) or `evidence`; quorum-affecting kinds (proposal/endorse/inhibit) are not accepted here.

```json
{ "topic_id": "<uuid>", "author": "<uuid>", "kind": "note", "summary": "…" }
```

Status: `200` on success; `404` if the topic is not tracked; `400` on malformed UUID or empty `summary`.

**POST /v1/longhouse/revoke** — operator hard-pull of a live title (OCEAN-246/272). The daemon presents its own server-minted `RevokerKey` — the caller names the title, but the daemon executes the revocation.

```json
{ "title_id": "<uuid>", "reason": "unsafe tool call" }
```

Status: `200` on success; `404` unknown title; `409` already revoked/released (`NotLive`); `400` malformed UUID.

**POST /v1/longhouse/recall** — cast a no-confidence vote in a seated firekeeper (OCEAN-302, quorum-of-recall). The daemon tallies distinct credentialed votes; when the tally carries (≥ threshold distinct voters) it pulls the title. A single voter casting multiple times counts as one vote.

```json
{ "topic_id": "<uuid>", "firekeeper_id": "<uuid>", "voter_id": "<uuid>", "threshold": 3 }
```

Status: `200` with `{ carried: false, votes, threshold }` while pending; `200` with `{ carried: true, revocation }` when carried and title pulled; `404` if no live firekeeper title for `(topic_id, firekeeper_id)`; `400` on malformed UUID. `threshold` is fixed on the first vote and ignored by subsequent votes.

**POST /v1/longhouse/breach** — report a detected policy breach against a seated title (OCEAN-302, policy-breach trigger). Each report accrues a graduated strike; the daemon escalates to a hard revoke at 3 strikes.

```json
{ "title_id": "<uuid>", "detail": "acted outside bound decision" }
```

Status: `200` with `{ revoked: false, strikes, threshold }` while below threshold; `200` with `{ revoked: true, revocation }` when the gradient tips and the title is pulled; `404` unknown title; `409` already revoked/released; `400` malformed UUID.

## Logs and events

### Process logs

Foreground daemon logs go to stderr/stdout through `tracing_subscriber`. Use `RUST_LOG` to adjust verbosity:

```bash
(cd "$HOME" && RUST_LOG=ocean_daemon=debug "$OCEAN_DAEMON_BIN")
```

If running under a user service, inspect the relevant journal unit, for example:

```bash
systemctl --user status <ocean-service-name>
journalctl --user -u <ocean-service-name> -f
```

Use the actual unit name from the local install; this repo does not currently define a canonical unit file.

### SSE event stream

Follow daemon events (modern session-scoped stream):

```bash
curl -N http://127.0.0.1:4780/v1/agent/events
# Scope to one session:
curl -N "http://127.0.0.1:4780/v1/agent/events?session_id=<id>"
```

The legacy global stream at `GET /v1/events` is still served for debug use but is not the recommended surface for production clients.

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
curl -N http://127.0.0.1:4780/v1/agent/events
```

Look for a request in `waiting_for_permission` and a matching `permission_request` event. Decide only with explicit operator approval.

### Prompt/session history looks wrong

Same-session execution is serialized. If history still looks wrong, capture the request ID, session ID, command, and logs before handing off; check for persistence errors or incorrect workspace/session selection rather than assuming a concurrent-write race.

### TUI is stale or blocked

- Verify the daemon URL with `OCEAN_DAEMON_URL` or `--url`.
- Confirm `/health` and the session-scoped `/v1/agent/events?session_id=<id>` stream work via curl.
- The retired mesh mode is not a supported recovery path.

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
