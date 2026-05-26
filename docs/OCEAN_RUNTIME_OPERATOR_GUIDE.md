# Ocean runtime operator guide

This guide is for operators running the current `ocean-rs` Rust-native Pi-style coding-agent harness/runtime and its clients. It reflects the repo state as validated from source on 2026-05-25.

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

Keep `OCEAN_BIND` loopback-only unless the operator has explicitly approved remote exposure and a security layer. The current daemon permits broad CORS and should be treated as local-only.

## Configuration

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

`ocean-agent` reads model config in this order:

1. `OCEAN_MODEL`
2. `PI_MODEL`
3. default `deepseek-chat`

Currently mapped model IDs include:

- `deepseek` / `deepseek-chat`
- `deepseek-v4-flash`
- `deepseek-reasoner` / `deepseek-r1`
- `gpt-4o`
- `gpt-4o-mini`
- `claude-sonnet-4-6` / `claude-sonnet` / `sonnet`
- `claude-opus-4-7` / `claude-opus` / `opus`

Any other model ID falls through to an OpenAI-compatible provider using `OCEAN_OPENAI_BASE_URL` or `https://api.openai.com/v1`.

Historical note: earlier audits found `deepseek-v4-flash` falling through to the generic OpenAI-compatible path. The Rev-gated task-1 hotfix maps `deepseek-v4-flash` explicitly. If a prompt reports a missing OpenAI key while you expected DeepSeek, re-check the deployed binary/service environment and confirm it includes that hotfix.

### API keys

For DeepSeek models, `ocean-agent` looks for:

1. `DEEPSEEK_API_KEY`
2. `~/.pi/agent/auth.json` at JSON pointer `/deepseek/key`

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

Current CLI caveat: the CLI can print `ok=false` from the daemon yet still exit successfully. Operators must inspect the footer until CLI exit-code behavior is hardened.

### Sessions

```bash
cargo run -p ocean-cli -- sessions
curl http://127.0.0.1:4780/v1/sessions
```

### TUI steering client

Default daemon steering client:

```bash
cargo run -p ocean-tui
```

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

Current daemon routes from source:

```text
GET  /
GET  /health
GET  /v1/events
POST /v1/prompt
GET  /v1/requests
POST /v1/requests
POST /v1/requests/{id}/cancel
POST /v1/permissions/{id}/decision
GET  /v1/sessions
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

Inspect stderr/stdout and the footer. Current CLI behavior may still exit with status 0 for an `ok=false` daemon response, so scripts should parse the JSON API or footer until fixed.

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
