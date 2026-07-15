# Ocean for Herdr

A local [Herdr](https://herdr.dev/) workflow plugin that opens Ocean in a
Herdr-managed tab. The Ocean TUI reports its authoritative lifecycle directly
to the surrounding pane, so Herdr shows `ocean` alongside other agents in the
sidebar.

```text
Herdr plugin action → managed terminal pane → ocean TUI → Ocean daemon :4780
                                              └→ Herdr pane lifecycle reports
```

This is intentionally not an ACP client. Herdr hosts a real terminal process;
Ocean continues to own its daemon session, transcript, tools, permissions, and
turn execution.

## Requirements

- Herdr `0.7.0` or newer.
- A current `ocean` or `ocean-tui` binary on `PATH`, built from this repository.
- Python 3 for the workspace-aware `Start Ocean` action.
- The Ocean daemon available normally; the TUI's existing local autostart policy
  still applies.

Override binary or daemon discovery before invoking the action when needed:

```bash
export OCEAN_BIN="$HOME/ocean-os/target/release/ocean-tui"
export OCEAN_DAEMON_URL="http://127.0.0.1:4780"
```

## Local development

From the `ocean-os` checkout:

```bash
cargo build -p ocean-tui --release
herdr plugin link ./integrations/herdr
herdr plugin action invoke risingtides.ocean.start
```

The action reads Herdr's invocation context, opens a managed Ocean tab in the
invoking workspace, and preserves that workspace's cwd.

You can also open the pane entrypoint directly:

```bash
herdr plugin pane open \
  --plugin risingtides.ocean \
  --entrypoint ocean \
  --placement tab \
  --cwd "$PWD" \
  --focus
```

Remove the development link without deleting files:

```bash
herdr plugin unlink risingtides.ocean
```

## GitHub installation

After this directory is published on GitHub, install it by repository subpath:

```bash
herdr plugin install risingtides-dev/ocean-os/integrations/herdr
```

Reinstall the same source to refresh a managed checkout. Uninstall with:

```bash
herdr plugin uninstall risingtides.ocean
```

## Optional keybinding

```toml
[[keys.command]]
key = "prefix+o"
type = "plugin_action"
command = "risingtides.ocean.start"
description = "start Ocean"
```

Reload Herdr configuration after editing it:

```bash
herdr server reload-config
```

## Lifecycle mapping

| Ocean lifecycle | Herdr state |
|---|---|
| TUI open, chooser, or completed turn | `idle` |
| Prompt submitted or `TurnStarted` | `working` |
| Current-session permission request | `blocked` |
| Permission resolved while the turn continues | `working` |
| `TurnFinished`, send failure, or outcome unknown | `idle` |
| TUI exit | lifecycle authority released |

State reports use Herdr's injected `HERDR_BIN_PATH` and `HERDR_PANE_ID`, run
best-effort off the TUI event loop, contain no prompts/tool arguments/decision
tokens, and are sequence-numbered so late subprocess completion cannot overwrite
a newer state. Exit waits at most 300 ms for the lifecycle release command so
normal shutdown does not leave stale authority behind.

Herdr currently reserves native restart/session restoration for its official
integrations. This plugin supplies first-class sidebar identity and lifecycle
status, but after a full Herdr server restart Ocean uses its own explicit
`ocean --session <id-or-prefix>` resume path rather than Herdr-native restore.

## Validation

```bash
python3 -m unittest integrations/herdr/test_start.py
sh -n integrations/herdr/run-ocean.sh
cargo test -p ocean-tui herdr::tests
```
