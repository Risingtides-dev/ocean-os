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
- A tool-call batch runs through the concurrency scheduler (`agent_loop.rs`): consecutive `Concurrency::Shared` (read-only) tools run in one concurrent segment; a `Concurrency::Exclusive` tool (the default) is a full barrier. A new tool defaults to `Exclusive` — only override `concurrency()` to `Shared` when the tool is genuinely side-effect-free. The persisted transcript MUST stay in original batch order regardless of finish order (provider tool_use/tool_result pairing depends on it).
- Blocking await boundaries in the agent loop (provider stream read, tool execution, retry backoff) must race the cancel token pre-yield via a biased `tokio::select!` on `cancelled(config)` — never a post-yield `is_cancelled` poll, which ignores user Halt on a silent socket until a wall-clock bound fires. Regression: `halt_during_silent_provider_stream_cancels_promptly` in `tests/agent_loop_e2e.rs`.
- Every provider round in a bound agent session must copy
  `AgentConfig::session_id` into `StreamOptions::session_id`; providers use that
  stable identity for cross-round prompt caching and request correlation.


## Work Guidance

- Prefer small, auditable tool-execution changes.
- Include tests for permission-sensitive behavior.
- Coordinate event shape changes with `ocean-daemon`, `ocean-core`, and `ocean-tui`.

## Verification

- `cargo test -p ocean-runtime`
- `cargo check --workspace`

## Child devlog Index

No child boundaries defined within `ocean-runtime/` at this time.
