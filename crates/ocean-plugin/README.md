# ocean-plugin

A subprocess-first plugin runtime for Ocean agent skill packs.

A **plugin** is a self-contained skill pack: a directory with a `plugin.toml`
manifest plus an executable that speaks **JSON-RPC 2.0 over stdio**. This crate
provides the runtime that discovers, launches, and drives plugins, and the
adapter (`PluginProvider`, behind the default `runtime` feature) that surfaces a
plugin's tools into `ocean-runtime`'s `CapabilityRegistry` — the same tool
surface as the built-ins and MCP tools.

The daemon's discovery + registration of plugins (OCEAN-110) lives in
`ocean-agent` (`discover_plugin_providers`); it scans `<config_dir>/plugins/`
(or `$OCEAN_PLUGINS_DIR`), parses each manifest, launches each plugin, and
namespaces its tools as `plugin__<name>__<tool>` — permission-gated like `bash`.

## Key types

- `Plugin` — the capability seam (`name`, `version`, `list_tools`, `invoke_tool`).
- `PluginManifest` — the parsed `plugin.toml` (name, version, entry, tools).
- `SubprocessPlugin` — a `Plugin` backed by a child process over JSON-RPC stdio.
- `PluginProvider` (feature `runtime`) — adapts a `Plugin` to the runtime's
  `CapabilityProvider` seam.

## Example plugin

A complete, runnable reference plugin lives at
[`examples/example_plugin/`](examples/example_plugin/) — a Rust binary exposing
`reverse_text` and `current_time`, with its `plugin.toml`. Build and smoke-test:

```bash
cargo build --example ocean-example-plugin -p ocean-plugin
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"list_tools"}' \
  | ./target/debug/examples/ocean-example-plugin
```

End-to-end tests that launch the example as a real child process and round-trip
its tools through `SubprocessPlugin` and `PluginProvider` are in
[`tests/example_plugin_e2e.rs`](tests/example_plugin_e2e.rs).

## Authoring guide

See [`docs/PLUGINS.md`](../../docs/PLUGINS.md) for the full guide: the
`plugin.toml` shape, the JSON-RPC stdio protocol (with example messages), where
plugins live, how the daemon discovers + registers them, tool namespacing +
permission-gating, and a step-by-step local-testing walkthrough.
