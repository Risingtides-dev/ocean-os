# Folder-as-Agent — spec

> **Ownership note (2026-07-13):** folder discovery, named-agent steering, and tool narrowing remain shipped compatibility behavior. The `subagents/` tree and `AgentDef.subagents` field are metadata only; core does not dispatch them. Any future subagent definition, spawn/join lifecycle, or orchestration is extension-owned and must not add a daemon/runtime-native task scheduler.

## Context

Today Ocean derives an agent's identity from the **surface** it's invoked
through: `client_type` (TUI/WEB/CLI/voice/…) maps to
`assistants/<DIR>/system.md`, composed onto the compiled `BASE_SYSTEM_PROMPT`
(`crates/ocean-agent/src/lib.rs`, `build_system_prompt_from` /
`load_surface_profile_from`). There is no notion of a *named agent* with its own
config, tools, skills, and child agents.

The goal: make an agent a **folder on disk** — the way an eve.dev / Next.js app
is a folder — Rust-native, read by the daemon, with identity derived from the
path. A coworker authors an agent by dropping files in a directory; the daemon
classifies and resolves it when invoked. No `agent.ts`, no TypeScript — Rust
reads `agent.toml` + `instructions.md`.

## Layout

```text
agents/
  <name>/
    agent.toml        runtime config: model, description, tools, capabilities, yolo
    instructions.md   base system prompt (the only required slot)
    skills/*.md       on-demand procedures, discovered by filename
    tools/*           tool allowlist entries, discovered by filename stem
    subagents/<id>/   nested agents, same shape, recursive
```

Identity comes from the path: `agents/researcher/` IS the agent `researcher`.
No `name`/`id` field is ever written in a file (matches eve's naming rule). A
folder with neither `instructions.md` nor `agent.toml` is not an agent.

Implemented in `crates/ocean-agent/src/agentdir.rs`:

- `discover(root) -> Vec<String>` — list agent names under a root
- `resolve(root, name) -> Result<AgentDef, ResolveError>` — the daemon's
  classification entry point; walks one folder into an `AgentDef`
- `AgentDef::system_prompt()` — the agent's `instructions.md`, or `None` →
  caller falls back to the compiled base prompt
- `AgentDef::effective_tools()` — `agent.toml` `tools` merged with `tools/`
  filename stems

Hot-read: the daemon reads the tree live, so editing a prompt is picked up on
the next turn — same contract as today's surface profiles. No build step.

## The capability question: can a crate be sideloaded without a daemon rebuild?

Yes — but **not** by `dlopen`-ing a Rust `rlib`. Rust has no stable ABI, so
loading compiled Rust straight into the daemon process is the unsafe/fragile
path and is rejected. Instead, every source of tools is a
`CapabilityProvider` (`crates/ocean-runtime/src/capability.rs`) folded into the
per-session `CapabilityRegistry`. The daemon binary stays frozen; it just
discovers more providers at runtime. Three sideload tiers, cheapest first:

| Tier | What | Rebuild daemon? | Machinery (exists today) |
| ---- | ---- | --------------- | ------------------------ |
| 0 — data agent | `agent.toml` + `instructions.md`, binds built-in tools by name | No | `agentdir` (this spec) + built-in tools |
| 1 — subprocess capability | a crate-as-binary (or any language) speaking JSON-RPC over stdio; tools fold in via `SubprocessPlugin` | No — separate compilation unit | `ocean-plugin/src/subprocess.rs`, `transport.rs` |
| 1b — subprocess *provider* | a whole agent CLI (`claude -p`, `codex exec`) as a model provider, deep mode | No | `docs/specs/subprocess-provider.md` |
| 2 — WASM skill pack | compiled tools as `.wasm`, run in a `wasmtime` sandbox in-process | No | `ocean-plugin/src/lib.rs` (architected; `wasm` feature pending) |

`agent.toml`'s `capabilities` field is the declared binding contract:

```toml
model = "anthropic/claude-opus-4.8"
description = "deep researcher"
tools = ["web_fetch"]                 # tier 0 — built-in, by name
capabilities = [
  "builtin:web_fetch",                # tier 0
  "mcp:linear",                       # configured MCP server (ocean-mcp)
  "subprocess:./tools/scrape",        # tier 1 — sideloaded crate-as-binary
  "wasm:./skills/extract.wasm",       # tier 2 — sideloaded sandboxed module
]
```

`capabilities` is parsed and surfaced now; the resolver does **not** spawn
anything. The daemon binds `builtin:`/`mcp:` against today's registry and wires
`subprocess:`/`wasm:` as those `ocean-plugin` lanes land. The field is the
contract, not a loader.

## Why this split (agents = data, capabilities = crates)

If every agent were literally a compiled Rust crate, adding an agent would mean
`cargo build` + daemon restart — killing the filesystem-first, edit-and-next-turn
hot-read that makes the model good. So: **agents are data** (a folder anyone can
author and download), **capabilities are crates** (the compiled tool surface the
agent binds to). A specialist that genuinely needs compiled Rust ships as a
tier-1 subprocess binary or tier-2 WASM pack — sideloaded, never recompiling the
daemon.

## What's built vs. next

**Built (shipped 2026-06-24):**

- `agentdir` resolver + config + discovery + traversal guard, unit-tested
  (`cargo test -p ocean-agent agentdir`).
- Daemon classification: the `agents/` root (env `OCEAN_AGENTS_DIR`, else
  `<config>/agents`), `GET /v1/agents` (returns summaries —
  `{name, description, model, skills, subagents}` — for a picker) and
  `GET /v1/agents/{name}` (full resolved def).
- The `agent` field on `AgentTurnRequest`: when set, the named agent's
  `instructions.md` is prepended as a steering layer on the turn (fail-open,
  behavior-neutral when absent).
- A tested reference agent at `docs/examples/agents/researcher/` (resolved by the
  `shipped_example_agent_resolves` test) to copy-and-adapt.
- Tool narrowing: a named agent's `tools` allowlist restricts the turn's toolset
  (fail-safe to the full set if it matches nothing). `narrow_tools` + the
  `tool_allowlist` channel on `PromptControl`.

- Model-honoring: a named agent's `agent.toml` `model` drives the turn when the
  request didn't pin a `model_id`. Fail-soft — an unresolvable model falls back
  to the global one + a warn (vs explicit `model_id` which fails hard). Uses
  Ocean's bare model aliases (`claude-opus-4-7`, see `GET /v1/models`).

- Capability binding — **tier 1 (subprocess), shipped (A2):** an agent folder's
  declared `[[subprocess_capability]]` entries are launched per-turn as
  `ocean-plugin` `SubprocessPlugin`s and merged into that turn's
  `CapabilityRegistry`, so the agent's declared tools become callable alongside
  the built-ins (namespaced `plugin__<name>__<tool>`, permission-gated). Each
  entry is a concrete, launchable spec (distinct from the forward-declared
  `capabilities` scheme-strings):

  ```toml
  [[subprocess_capability]]
  name = "scrape"                  # namespaces its tools; defaults to command stem
  command = "./tools/scrape"       # relative entries resolve against the agent folder
  args = ["--stdio"]
  cwd = "."                        # optional
  env = { API_BASE = "https://…" } # optional extra child env
  ```

  Fail-soft: a spec whose command can't spawn is logged and skipped — it never
  kills the turn (mirrors the model-honoring fail-soft path). Reuses
  `ocean-plugin`'s subprocess JSON-RPC wholesale. Bound at the `ocean-agent` turn
  layer via `PromptControl::with_agent_capabilities`; the daemon reads the caps
  off the resolved `AgentDef` and threads them onto the turn.

**Next (separate PRs):**

1. Capability binding, remaining tiers: `builtin:`/`mcp:` scheme-strings against
   today's registry; tier-2 `wasm:` sideloaded as that `ocean-plugin` lane lands.
2. Map eve-style gateway model ids (`anthropic/claude-opus-4.8`) to Ocean aliases
   so `agent.toml` `model` can accept either format.
