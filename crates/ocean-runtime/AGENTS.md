# ocean-runtime — Agent Loop and Tools

## Purpose

This crate owns the Ocean agent loop and permission-gated tool execution runtime.

## Ownership

- **Scope:** `crates/ocean-runtime/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Primary responsibilities:** agent turn execution, tool dispatch, permission boundaries, runtime event production

## Local Contracts

- Permission gates are mandatory; do not add execution paths that bypass them.
- Built-in filesystem/process tools must resolve relative paths and shell commands against the turn's `SessionContext.cwd`, not the daemon process cwd.
- Tool-using turns must reserve a final synthesis path: do not let repeated tool calls consume the entire turn budget without a user-visible assistant reply.
- Assistant text present in a provider's terminal message must be emitted as `TextDelta` when the provider did not stream text chunks, so SSE clients always render the final reply.
- Runtime events must remain compatible with `ocean-core` event contracts and daemon SSE streaming.
- Keep provider concerns outside runtime unless mediated through the protocol/provider layers.

## Work Guidance

- Prefer small, auditable tool-execution changes.
- Include tests for permission-sensitive behavior.
- Coordinate event shape changes with `ocean-daemon`, `ocean-core`, and `ocean-tui`.

## Verification

- `cargo test -p ocean-runtime`
- `cargo check --workspace`

## Child devlog Index

No child boundaries defined within `ocean-runtime/` at this time.
