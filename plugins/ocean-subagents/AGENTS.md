# ocean-subagents — Ocean Tool Plugin

## Purpose

Provide working, permission-gated subagent tools inside ordinary Ocean turns by dispatching bounded child turns through the existing daemon agent APIs.

## Ownership

- `ocean-subagents.py` owns plugin JSON-RPC, durable run metadata, lifecycle refresh, output projection, follow-up turns, token-bound child permission decisions, cancellation, and elapsed-time watchdogs.
- `plugin.toml` owns the model-visible tool contract.
- `agent/ocean-subagent-worker/` owns the fixed child profile that excludes the subagent plugin and prevents recursive delegation.
- `install.sh` installs only this plugin and fixed worker agent under the Ocean config root.

## Local Contracts

- Use `POST /v1/agent/turns`; never call a provider directly.
- Spawn returns immediately with durable run, turn, and session identifiers.
- Child execution remains an ordinary Ocean session with normal tool permissions; every child turn carries a private decision token, and the plugin accepts permission decisions only for the exact run/request/session/tool tuple.
- Enforce fixed worker-profile binding, maximum four active runs, bounded output, and an elapsed-time watchdog.
- Persist metadata atomically under `~/.local/state/ocean/subagents` by default.
- Do not claim exactly-once execution; daemon request/session truth wins during refresh.

## Work Guidance

- Keep the implementation Python-standard-library only.
- Stdout is exclusively JSON-RPC; diagnostics go to stderr.
- Keep schemas and live `list_tools` definitions identical.

## Verification

- `python3 -m unittest plugins/ocean-subagents/test_ocean_subagents.py`
- `python3 plugins/ocean-subagents/test_wire.py`
- `python3 -m py_compile plugins/ocean-subagents/ocean-subagents.py`
- `sh -n plugins/ocean-subagents/install.sh`

## Child devlog Index

No child devlogs.
