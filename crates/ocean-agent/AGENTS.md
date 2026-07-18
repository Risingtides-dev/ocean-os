# ocean-agent — Session and Prompt Layer

## Purpose

This crate owns Ocean's agent session/history layer and project prompt loading. Session load/save bugs here affect both the TUI and `ocean-surface` because clients depend on the daemon remembering transcripts by session id.

## Ownership

- **Scope:** `crates/ocean-agent/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Primary responsibilities:** session persistence, workspace binding, GC, and transcript projection in `src/session/mod.rs`; runtime/history shaping in `src/lib.rs`; system/surface prompt assembly, project instruction discovery, and prompt-memory context in `src/system_prompt.rs`

## Local Contracts

- Preserve session compatibility unless a migration is documented.
- Session-config model pins update model/provider together under the same
  per-session lock as turn persistence. Optional config reads must distinguish
  an absent session from unreadable/corrupt storage so daemon adapters map only
  genuine absence to 404.
- Permission-mode persistence atomically writes the authoritative three-state
  file and reports write failures. Load old booleans as automatic/skip-all; the
  legacy `yolo_pref` is a best-effort downgrade mirror, while current boolean
  reads derive from the authoritative mode so the two cannot disagree live.
- Project instruction discovery must respect the repo devlog chain: repo-root `AGENTS.md` is the root contract; `.ocean/AGENTS.md` is only a child doc for `.ocean/` runtime artifacts.
- Do not add new instruction sources without tests proving ancestor/nested cwd behavior.
- Turn persistence is incremental: save the accepted user message before provider execution, then save only at provider-valid round boundaries where every assistant tool call has its ordered tool result. Never persist an orphan tool-call batch.
- Spawned agent loops must remain owned by the parent turn future. Dropping the parent must abort the child; Tokio's default detached-on-`JoinHandle`-drop behavior is unsafe for side-effecting tools.
- Pre-stream provider failover must pin one session id and hold one per-session turn lock across the complete primary/fallback transaction, reusing the primary attempt's durable accepted-user row; never allow an intervening turn, append the operator prompt twice, or orphan an acceptance-only session.
- Track-0 room prompt guidance is retired; prompt assembly must not infer a closed room role from agent-turn input.
- Persisted history search reads only display-projected user/assistant transcript text; it must never inspect tool payloads/raw provider messages or invoke providers/embeddings. Preflight cumulative raw session-file size against the 64 MiB request budget, then enforce the same cumulative bound while reading so concurrent replacement/growth cannot bypass it.
- `PromptControl::without_tools()` is the fail-closed no-capabilities boundary. Empty or unmatched folder-agent allowlists intentionally remain fail-open and must never represent a no-tools posture.
- `PromptControl` receives exactly two effective harness-profile booleans from the daemon: `hashline_edits` and `artifact_spill`. Direct/legacy callers default both off; do not add declarative profile fields here until production runtime composition actually consumes them.
- History shaping preserves stored thinking only when the selected route is exact
  `kimi`/`kimi-k3` (Moonshot requires same-model `reasoning_content` replay) or
  `openai-codex` (the codex encoder replays its own marker-signed encrypted
  reasoning items and MUST receive them back — stripping them degenerates
  gpt-5.x into malformed tool calls across tool rounds). Kimi K2.x and other
  OpenAI-compatible routes retain the existing thinking-strip boundary;
  provider encoders still drop cross-provider thinking.
- `compact_session` is owned here: one-shot no-tools model call, atomically
  replaces session transcript with summary + protected recent window. The
  session lock must be held for the entire load-call-save cycle. Only the
  current-runtime model is used; session-historical model is ignored. The
  protected window keeps at most 20 messages and at most 20% of the context
  window (always the newest message) and never begins on an orphan tool
  result. A fully-protected transcript is an `ok:true` no-op with no model
  call. Provider readiness fails closed before the call; the call is bounded
  by the 300-second turn budget; every failure path (not-ready, provider
  error, timeout, empty summary) leaves the stored transcript untouched, and
  corrupt storage is an `Err`, never a wipe.

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
