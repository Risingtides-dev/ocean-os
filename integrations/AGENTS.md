# integrations/ — External Host Adapters

## Purpose

This directory owns small distributable adapters that launch or connect Ocean
through third-party host extension surfaces. These adapters remain clients of
Ocean OS; they do not become alternate agent runtimes.

## Ownership

- **Scope:** `integrations/`
- **Parent contract:** `../AGENTS.md`
- **Runtime authority:** remains in the daemon, `ocean-agent`, and
  `ocean-runtime`

## Local Contracts

- Read the target host's current public extension documentation before changing
  its adapter.
- Keep host manifests and launcher code thin. Provider calls, sessions,
  permissions, tools, and transcripts remain daemon-owned.
- Host lifecycle projections must derive from authoritative Ocean client events,
  fail open, and never block a turn or terminal event loop.
- Never label these packages as Ocean daemon tool plugins; `crates/ocean-plugin`
  owns that separate subprocess ABI.
- After a meaningful change, refresh this index and append a root `events.md`
  entry with `worktree:`.

## Adapter Index

| Path | Host surface | Entry point | Narrow validation |
|---|---|---|---|
| `herdr/` | Herdr workflow plugin and managed Ocean pane | `herdr-plugin.toml`, `start.py`, `run-ocean.sh` | `python3 -m unittest integrations/herdr/test_start.py && sh -n integrations/herdr/run-ocean.sh` |

## Child Devlog Index

No child `AGENTS.md` boundaries are currently defined.
