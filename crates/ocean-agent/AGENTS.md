# ocean-agent — Session and Prompt Layer

## Purpose

This crate owns Ocean's agent session/history layer and project prompt loading. Session load/save bugs here affect both the TUI and `ocean-surface` because clients depend on the daemon remembering transcripts by session id.

## Ownership

- **Scope:** `crates/ocean-agent/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Primary responsibilities:** session persistence, workspace binding, GC, and transcript projection in `src/session/mod.rs`; runtime/history shaping in `src/lib.rs`; system/surface prompt assembly, project instruction discovery, and prompt-memory context in `src/system_prompt.rs`

## Local Contracts

- Preserve session compatibility unless a migration is documented.
- Project instruction discovery must respect the repo devlog chain: repo-root `AGENTS.md` is the root contract; `.ocean/AGENTS.md` is only a child doc for `.ocean/` runtime artifacts.
- Do not add new instruction sources without tests proving ancestor/nested cwd behavior.
- Turn persistence is incremental: save the accepted user message before provider execution, then save only at provider-valid round boundaries where every assistant tool call has its ordered tool result. Never persist an orphan tool-call batch.
- Spawned agent loops must remain owned by the parent turn future. Dropping the parent must abort the child; Tokio's default detached-on-`JoinHandle`-drop behavior is unsafe for side-effecting tools.
- Pre-stream provider failover must pin one session id and hold one per-session turn lock across the complete primary/fallback transaction, reusing the primary attempt's durable accepted-user row; never allow an intervening turn, append the operator prompt twice, or orphan an acceptance-only session.
- Track-0 room prompt guidance is retired; prompt assembly must not infer a closed room role from agent-turn input.

## Work Guidance

- Keep prompt-loading behavior deterministic and easy for cold agents to reason about.
- `src/system_prompt.rs` is one intact cohesion boundary. Prompt wording and literal bytes are behavior; do not mix wording changes with structural extraction.
- `src/session/mod.rs` is the intact persistence boundary. Do not split it or change schema, atomic-save order, duplicate healing, or resume behavior without a separately approved design and compatibility tests.
- Avoid client-specific assumptions; daemon, TUI, and surface clients share this session layer.
- Refresh the recorded `cwd` on every bind; update `workspace_root` and git metadata when the caller moves into a different workspace.
- When changing prompt text, include tests for client-type differences when relevant.
- The TUI fallback/profile guidance must advertise its supported terminal component
  projections and distinguish them from unsupported arbitrary web/HTML layouts;
  never restore a blanket `component_render` ban while the TUI consumes those events.
- Keep the base prompt compact and tool-agnostic: runtime tool schemas describe mechanics; the prompt governs selection, batching, and verification.
- Memory guidance must not encourage unconditional recall. Call `recall` only when prior conversations, preferences, or decisions are needed and not already injected.

## Verification

- `cargo test -p ocean-agent system_prompt`
- `cargo test -p ocean-agent session`
- `cargo test -p ocean-agent project_prompt_loads_ocean_agents_md_from_ancestor`
- `cargo test -p ocean-agent`
- `cargo check --workspace`

## Child devlog Index

No child boundaries defined within `ocean-agent/` at this time.
