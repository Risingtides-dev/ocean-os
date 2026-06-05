# ocean-heartbeat

`ocean-heartbeat` is the Rust runner for scheduled Ocean routines. The first use case is a prompt-injection heartbeat that posts a scoped turn to the local daemon, but the shape is meant to grow into courier jobs, specialist-agent setup flows, and eventually a dashboard of scheduled automations.

## Current docs-site routine

Config:

```text
docs/ocean-heartbeat/ocean-site-docs.toml
```

Build/check the runner:

```bash
cargo check -p ocean-heartbeat
cargo build -p ocean-heartbeat --release
```

Run one heartbeat manually:

```bash
./target/release/ocean-heartbeat \
  --daemon-url http://127.0.0.1:4780 \
  run --config docs/ocean-heartbeat/ocean-site-docs.toml
```

Generate a render-protocol component payload for a PWA/dashboard view:

```bash
./target/release/ocean-heartbeat \
  component --config docs/ocean-heartbeat/ocean-site-docs.toml
```

This prints an AgentTurnEvent-like `component_render` JSON object with a `stat` component payload. The future Ocean Surface scheduler dashboard can either consume that envelope directly or call the daemon endpoint that eventually wraps these routine snapshots into the normal `/v1/agent/events` stream.

Generate a macOS launchd plist without installing it:

```bash
./target/release/ocean-heartbeat \
  --daemon-url http://127.0.0.1:4780 \
  launchd \
  --config docs/ocean-heartbeat/ocean-site-docs.toml \
  --bin /Users/risingtidesdev/dev/ocean-os/target/release/ocean-heartbeat \
  --every-seconds 3600 \
  > /tmp/dev.risingtides.ocean-site-docs.plist
```

Review the generated plist before installing. A later slice should add first-class `install-launchd`, `uninstall-launchd`, and `list` commands with explicit confirmation semantics.

## Why replace the shell script

The old `scripts/ocean-site-heartbeat.sh` LaunchAgent exists, but launchd reported exit `127` with a brittle shell/heredoc failure. The Rust runner avoids shell JSON construction and uses `reqwest` to call:

```text
POST /v1/agent/turns
```

It persists the returned `session_id` into the configured `session_file` so future scheduled turns continue the same daemon-side session.
