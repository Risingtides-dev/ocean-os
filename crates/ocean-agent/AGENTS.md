# ocean-agent — Session and Prompt Layer

## Purpose

This crate owns Ocean's agent session/history layer and project prompt loading. Session load/save bugs here affect both the TUI and `ocean-surface` because clients depend on the daemon remembering transcripts by session id.

## Ownership

- **Scope:** `crates/ocean-agent/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Primary responsibilities:** session persistence, history shaping, prompt assembly, project instruction discovery

## Local Contracts

- Preserve session compatibility unless a migration is documented.
- Project instruction discovery must respect the repo devlog chain: repo-root `AGENTS.md` is the root contract; `.ocean/AGENTS.md` is only a child doc for `.ocean/` runtime artifacts.
- Do not add new instruction sources without tests proving ancestor/nested cwd behavior.

## Work Guidance

- Keep prompt-loading behavior deterministic and easy for cold agents to reason about.
- Avoid client-specific assumptions; daemon, TUI, and surface clients share this session layer.
- Refresh the recorded `cwd` on every bind; update `workspace_root` and git metadata when the caller moves into a different workspace.
- When changing prompt text, include tests for client-type differences when relevant.
- Keep the base prompt compact and tool-agnostic: runtime tool schemas describe mechanics; the prompt governs selection, batching, and verification.
- Memory guidance must not encourage unconditional recall. Call `recall` only when prior conversations, preferences, or decisions are needed and not already injected.

## Verification

- `cargo test -p ocean-agent project_prompt_loads_ocean_agents_md_from_ancestor`
- `cargo test -p ocean-agent`
- `cargo check --workspace`

## Child devlog Index

No child boundaries defined within `ocean-agent/` at this time.
