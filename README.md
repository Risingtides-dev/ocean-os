# ocean-rs

Rust-native agent harness for Ocean OS.

This is **not** a Pi fork. We are using Pi concepts as reference material, then building a lower-level Rust runtime designed for:

- long-running local daemon
- OceanTUI / Ocean GUI clients
- low memory usage
- fast startup
- Rust-first tool execution
- distro-level integration

## Current bootstrapping phase

`ocean-daemon` exposes a local HTTP API and now runs the first native `ocean-agent` path in-process. The old `pi-rs-deepseek` wrapper has been taken apart: Ocean owns daemon config, DeepSeek model selection, key discovery, session persistence, permission defaults, and protocol mapping. The remaining Pi Rust crates are used only as small provider/agent/tool components while Ocean-native crates replace them piece by piece.

## Run

```bash
cargo run -p ocean-daemon
```

Health:

```bash
curl http://127.0.0.1:4780/health
```

Prompt:

```bash
cargo run -p ocean-cli -- prompt "Reply OK"
```

## Direction

See `docs/ARCHITECTURE.md` and `ROADMAP.md`.
