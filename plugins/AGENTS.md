# plugins/ — Ocean Tool Plugins

## Purpose

This directory owns distributable subprocess tool plugins loaded by `ocean-plugin` from the operator's Ocean config directory.

## Ownership

- Plugins speak the newline-delimited JSON-RPC ABI documented in `docs/PLUGINS.md`.
- The daemon remains session, model, permission, and tool-execution authority.
- Each child folder owns its manifest, executable, installer, tests, and operator documentation.

## Local Contracts

- Every plugin tool remains permission-gated by the existing Ocean runtime.
- Plugins may call documented daemon APIs but must not call providers directly or read provider credentials.
- Keep durable plugin state outside daemon session files.
- Installers must never modify the repository checkout or silently overwrite unrelated plugin/agent packages.

## Work Guidance

- Prefer portable standard-library implementations when a compiled binary is unnecessary.
- Bound fan-out, runtime, output, and persistent state.
- Verify the real stdio wire plus daemon-facing behavior.

## Verification

Run the narrow commands in the owning child contract and `cargo xtask docs-check`.

## Child devlog Index

- `ocean-subagents/` — permission-gated subagent lifecycle tools over ordinary Ocean child sessions → `ocean-subagents/AGENTS.md`
