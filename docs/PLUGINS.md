# Ocean Plugins — Authoring Guide

> How to write a plugin that the Ocean daemon discovers, loads, and exposes to
> the agent as permission-gated tools.
>
> A **plugin** is a self-contained skill pack: a directory holding a
> `plugin.toml` manifest plus an executable that speaks **JSON-RPC 2.0 over
> stdio**. The daemon scans a plugins directory, parses each manifest, launches
> each plugin as a child process, and surfaces its declared tools into the same
> tool registry as the built-ins (`bash`, `write`, …) and MCP tools — one unified
> tool surface, no special-casing.
>
> The runtime that drives plugins lives in `crates/ocean-plugin`. Discovery +
> registration (OCEAN-110) lives in `ocean-agent` (`discover_plugin_providers`).
> A complete, runnable reference plugin ships at
> `crates/ocean-plugin/examples/example_plugin/`.

## TL;DR

1. Write an executable that reads JSON-RPC requests on stdin and writes responses
   on stdout, answering two methods: `list_tools` and `invoke_tool`.
2. Drop it in a directory with a `plugin.toml` that names it and declares its
   tools.
3. Put that directory under `<config_dir>/plugins/` (or `$OCEAN_PLUGINS_DIR`).
4. Start the daemon. Your tools appear to the agent as
   `plugin__<plugin-name>__<tool>`, permission-gated like `bash`.

Everything below is the detail behind those four steps, ending with a copy-paste
walkthrough using the bundled example.

---

## 1. The `plugin.toml` manifest

The manifest is the **source of truth for discovery**: the daemon reads it
*before* launching anything, so it knows the plugin's identity and tools without
spawning a process. A launched plugin then re-confirms its tools live via
`list_tools`.

```toml
name = "example-plugin"        # stable id; namespaces tools as plugin__<name>__<tool>
version = "0.1.0"              # semver by convention (not enforced)
entry = "ocean-example-plugin" # launchable executable, relative to THIS file's dir
                               # (an absolute path is used as-is)

[[tool]]
name = "reverse_text"
description = "Reverse a string. Returns { reversed: <string> }."
# JSON Schema for the tool's arguments. `input_schema` (snake) is idiomatic;
# `inputSchema` (the JSON wire spelling) is also accepted as an alias.
input_schema = { type = "object", properties = { text = { type = "string" } }, required = ["text"] }

[[tool]]
name = "current_time"
description = "Current Unix time in seconds. Takes no arguments."
input_schema = { type = "object", properties = {} }
```

Fields:

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Stable identifier. Namespaces the plugin's tools. |
| `version` | yes | Version string (semver by convention, not enforced). |
| `entry` | yes | Path to the executable, **relative to the manifest's directory** (absolute used as-is). The daemon spawns this. |
| `[[tool]]` | 0+ | One entry per advertised tool: `name`, optional `description`, optional `input_schema` (defaults to `{}`). |

A missing required field (`name`, `version`, or `entry`) is a parse error and the
plugin is **skipped** (logged at warn) — it can never break daemon startup.

---

## 2. The JSON-RPC stdio protocol

The plugin and the runtime exchange **one JSON object per line**: the runtime
writes a request line to the plugin's stdin, the plugin writes a response line to
its stdout. (Stdout is reserved for the wire — log to **stderr** if you need to.)

There are exactly **two methods**, both request/response. The runtime never calls
back into the plugin, so a plugin only ever *answers*; it never *initiates*.

### `list_tools`

Request (no params):

```json
{"jsonrpc":"2.0","id":1,"method":"list_tools"}
```

Response — the tools this plugin offers. Each tool is `{ name, description?,
inputSchema? }` (note the JSON wire spelling `inputSchema`):

```json
{"jsonrpc":"2.0","id":1,"result":{"tools":[
  {"name":"reverse_text","description":"Reverse a string.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},
  {"name":"current_time","description":"Current Unix time in seconds.","inputSchema":{"type":"object","properties":{}}}
]}}
```

### `invoke_tool`

Request — `params` carries the tool `name` and its `args` object:

```json
{"jsonrpc":"2.0","id":2,"method":"invoke_tool","params":{"name":"reverse_text","args":{"text":"ocean"}}}
```

Response — the tool's **JSON result**, opaque to the runtime. Whatever shape you
return is handed to the model as the tool result (a JSON string passes through
verbatim; any other value is rendered as compact JSON):

```json
{"jsonrpc":"2.0","id":2,"result":{"reversed":"naeco"}}
```

### Errors

Return a JSON-RPC `error` object for an unknown method, an unknown tool, or bad
arguments. The runtime surfaces it to the model as an error tool result (not a
hard loop failure):

```json
{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"unknown tool: nope"}}
```

Conventions: `-32601` for unknown method/tool, `-32602` for invalid arguments.

### Notifications

A line with **no `id`** (or a null `id`) is a notification — the plugin must not
reply to it. In practice the runtime sends only id-carrying requests; just don't
emit unsolicited lines.

### Timeouts & lifecycle

- Each request has a per-request timeout (30s default). A silent plugin fails the
  call, not the whole daemon.
- The runtime multiplexes concurrent `invoke_tool` calls over the one stdio pipe
  and matches responses by `id`, so your plugin may receive overlapping requests.
  Reply to each with its own `id`. (Handling them serially in a read loop, like
  the example, is fine — just keep `id`s straight.)
- The plugin is killed on shutdown (kill-on-drop). Reaching EOF on stdin means
  the host is gone; exit your loop.

---

## 3. Where plugins live & how the daemon finds them

The daemon resolves the **plugins directory** as:

1. `$OCEAN_PLUGINS_DIR` if set, else
2. `<config_dir>/plugins/`

where `<config_dir>` is `$OCEAN_CONFIG_DIR`, else `$XDG_CONFIG_HOME/ocean-rs`,
else `~/.config/ocean-rs`, else `./.ocean-rs`. So by default plugins live in
`~/.config/ocean-rs/plugins/`.

**Each immediate subdirectory** of the plugins directory that contains a
`plugin.toml` is one plugin:

```
~/.config/ocean-rs/plugins/
├── example-plugin/
│   ├── plugin.toml
│   └── ocean-example-plugin      # the entry executable
└── my-other-plugin/
    ├── plugin.toml
    └── run.sh
```

Discovery is **fail-soft**, mirroring MCP server discovery:

- No plugins directory → no plugins, no error (the normal case).
- A manifest that fails to parse, or an entry that fails to spawn → that plugin is
  skipped (logged at warn). Other plugins still load.
- A plugin that launches but fails `list_tools` → it registers as an empty,
  `Unavailable` provider contributing no tools, rather than wedging a turn.

One broken plugin can never break daemon startup or another plugin.

> The discovery + registration code is in `ocean-agent`
> (`discover_plugin_providers` / `build_capability_registry`). The daemon calls
> it at registry-construction time; you do not edit the daemon to add a plugin —
> dropping a directory in the plugins dir is the entire install.

---

## 4. Tool namespacing & permissions

Every plugin tool is exposed to the agent under a namespaced id:

```
plugin__<plugin-name>__<tool>
```

e.g. `reverse_text` in `example-plugin` becomes
`plugin__example-plugin__reverse_text`. This guarantees a plugin tool can never
collide with a built-in (`bash`), an MCP tool (`mcp__…`), or another plugin. The
registry's first-wins dedup also makes built-ins unshadowable.

**Permission gating:** plugin tools run arbitrary out-of-process code, so they
**require approval by default** — exactly like `bash`, `write`, and `edit`. The
daemon's existing `PermissionPolicy` gates every plugin tool call. **Plugins
never bypass the permission gate.** (This is enforced in the runtime adapter:
every plugin tool reports `requires_permission() == true`.)

---

## 5. Local testing — step by step

This walks through the bundled example plugin
(`crates/ocean-plugin/examples/example_plugin/`), which exposes two tools:
`reverse_text` and `current_time`.

### 5a. Build the example

```bash
cargo build --release --example ocean-example-plugin -p ocean-plugin
# binary lands at: target/release/examples/ocean-example-plugin
```

### 5b. Smoke-test it on the wire (no daemon)

Pipe two requests straight into the binary:

```bash
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"list_tools"}' \
  '{"jsonrpc":"2.0","id":2,"method":"invoke_tool","params":{"name":"reverse_text","args":{"text":"hello"}}}' \
  | ./target/release/examples/ocean-example-plugin
```

Expect a `list_tools` response listing both tools, then
`{"jsonrpc":"2.0","id":2,"result":{"reversed":"olleh"}}`.

### 5c. Install it into the plugins directory

Copy the manifest and the built binary into a plugin directory. The manifest's
`entry = "ocean-example-plugin"` is relative to its own directory, so the binary
sits next to it:

```bash
PLUGIN_DIR="$HOME/.config/ocean-rs/plugins/example-plugin"   # or $OCEAN_PLUGINS_DIR/example-plugin
mkdir -p "$PLUGIN_DIR"
cp crates/ocean-plugin/examples/example_plugin/plugin.toml "$PLUGIN_DIR/"
cp target/release/examples/ocean-example-plugin "$PLUGIN_DIR/"
chmod +x "$PLUGIN_DIR/ocean-example-plugin"
```

(Or point `OCEAN_PLUGINS_DIR` at any directory and put `example-plugin/` there.)

### 5d. Start the daemon and confirm the tool loads

```bash
cargo build --workspace --release
./target/release/ocean-daemon
```

On startup the daemon logs `plugin ready` with the plugin name and tool count.
The tools `plugin__example-plugin__reverse_text` and
`plugin__example-plugin__current_time` are now in the agent's tool set.

### 5e. Run a tool

From the TUI (`ocean`) or any client, ask the agent to reverse some text — e.g.
*"use the example plugin to reverse the word 'ocean'"*. The agent calls
`plugin__example-plugin__reverse_text`; because plugin tools are permission-gated,
you'll be **prompted to approve** the call (just like `bash`). Approve it and the
result (`naeco`) comes back.

### 5f. Run the automated proof

The crate ships an end-to-end test that launches the example as a real child
process and round-trips both tools through `SubprocessPlugin`, then through the
runtime adapter (`PluginProvider`) to confirm namespacing + permission-gating:

```bash
cargo build --example ocean-example-plugin -p ocean-plugin
cargo test -p ocean-plugin
# tests/example_plugin_e2e.rs: example_manifest_parses,
#   example_subprocess_round_trip, example_loads_through_runtime
```

---

## Writing your own plugin

Any language works — the contract is just newline-delimited JSON-RPC on stdio.
The bundled example is a ~150-line Rust binary
(`crates/ocean-plugin/examples/example_plugin/main.rs`); a portable shell or
Python script that reads stdin lines and writes JSON responses works identically.
Checklist:

1. Read one JSON object per line from stdin; write one per line to stdout; flush
   after each. Keep stdout for the wire only — log to stderr.
2. Answer `list_tools` with your tool set (`name`, `description`, `inputSchema`).
3. Answer `invoke_tool` by dispatching on `params.name`, reading `params.args`,
   and returning a `result` (or an `error` for unknown tools / bad args).
4. Write a `plugin.toml` naming your executable as `entry` and declaring the same
   tools.
5. Drop the directory in `<config_dir>/plugins/` (or `$OCEAN_PLUGINS_DIR`) and
   restart the daemon.

## Reference

- Runtime + types: `crates/ocean-plugin/` — `Plugin` trait, `PluginManifest`,
  `SubprocessPlugin`, `PluginProvider`.
- Example plugin: `crates/ocean-plugin/examples/example_plugin/`.
- Discovery + registration (OCEAN-110): `crates/ocean-agent/src/lib.rs`
  (`discover_plugin_providers`, `plugins_dir`).
- End-to-end tests: `crates/ocean-plugin/tests/example_plugin_e2e.rs`.
