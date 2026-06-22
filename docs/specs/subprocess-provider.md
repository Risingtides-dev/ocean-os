# SubprocessProvider — spec

## Context

Ocean reaches models today by calling provider HTTP APIs directly
(`ocean-providers` → `api.anthropic.com`, Codex backend, etc.) and running its
own agent loop in `ocean-runtime`. That gives full control but means Ocean
re-implements everything an agent CLI already ships: tool execution,
permissions, session files, MCP wiring, compaction.

A **SubprocessProvider** is the second integration lane: spawn an existing agent
CLI (`claude -p --output-format stream-json`, later `codex exec --json`) as a
headless worker and stream its output back through Ocean's normal provider
contract. The payoff is provider-portability and inheriting each CLI's whole
agent harness for free. It also lets the Telegram bridge — currently a
standalone hand-rolled version of exactly this — collapse into a plain client of
`POST /v1/agent/turns`, with the daemon deciding whether a turn runs on the
native Anthropic API provider or a spawned `claude` worker.

This is a finish-the-wiring job, not a from-scratch build: the subprocess
transport already exists and the provider trait seam is clean.

## The one decision that shapes everything: thin vs. deep

`claude --output-format stream-json` is **not** an LLM token stream — it's the
output of a *complete agent*: it runs its own tool calls (Bash, Edit, etc.),
manages its own permissions, and emits events for all of it. Ocean's
`Provider::stream` contract, by contrast, expects a *model* that emits text /
thinking / tool-call deltas and then stops with `Done`, after which **Ocean's**
loop executes the tools and calls the provider again.

These two models don't compose naively. Two options:

- **Thin (rejected):** Map claude's `tool_use` events onto Ocean's
  `ToolCallEnd` and let Ocean's loop execute them. **Breaks** — claude has
  *already executed* the tool inside its own process. You'd get double
  execution and a fight over who owns the loop.

- **Deep (chosen):** Treat the spawned `claude` as a self-contained agent.
  Ocean's loop runs exactly **one iteration**: hand the prompt to the worker,
  stream its events through translated into `AssistantMessageEvent`, and finish
  with a single `Done`. Tool calls that happen inside claude are surfaced to
  Ocean as **informational** events (rendered as tool activity), not as
  `ToolCallEnd` requests Ocean is expected to fulfil. The worker owns its loop;
  Ocean owns the session/transport/surface.

Everything below assumes **deep**. The consequence: when a turn is routed to a
SubprocessProvider, Ocean must run it in **passthrough mode** — its own tool
registry and permission gate are inert for that turn, because the worker is
doing that work. This is a per-provider capability flag, specced in §6.

## Contract to implement

`ocean-protocol/src/providers/mod.rs:14`

```rust
#[async_trait]
impl Provider for SubprocessProvider {
    async fn stream(&self, model: &Model, context: &Context, options: &StreamOptions)
        -> Result<AssistantMessageEventStream>;
}
```

Emit the existing enum (`ocean-protocol/src/types.rs:295`): `Start` →
(`TextDelta` | `ThinkingDelta` | tool-activity) … → `Done { reason, message }`.
No new variant is strictly required for an MVP — tool activity from the worker
can render as thinking/text. A clean implementation later adds a dedicated
`ToolActivity` informational variant so surfaces can show "ran Bash(…)" without
it looking like a model thought. Start without it; add when the rendering
matters.

## Why we can't reuse `SubprocessPlugin` as-is

`ocean-plugin` already spawns subprocesses and speaks newline-delimited JSON
over stdio — but its router (`subprocess.rs` `route_inbound`) is hardwired to
JSON-RPC request/response: any line **without an `id` is dropped**
(`jsonrpc.rs` `is_response()`). claude's stream-json is a one-way event stream
with no request ids, so every event would be silently discarded.

**Reuse the transport, not the router.** `StdioTransport`
(`ocean-plugin/src/transport.rs:48`) is a clean, standalone
spawn-and-line-buffer adapter (`tokio::process`, `stdin/stdout` piped, stderr
inherited, `kill_on_drop(true)`). Use it directly; write our own event loop
instead of `SubprocessPlugin`'s request/response multiplexer.

## Components

### 1. `crates/ocean-providers-subprocess/` (new crate, or module in providers)

- `SubprocessProvider { binary: PathBuf, base_args: Vec<String> }`
- `impl Provider` — the meat:
  1. Build argv from `model` + `context`: `claude -p --output-format
     stream-json --verbose --include-partial-messages` plus `--add-dir`,
     `--allowedTools`, `--permission-mode bypassPermissions`, and `--resume
     <id>` / `--session-id <id>` for continuity (the Telegram bridge's
     `_build_cmd` in `telegram-bridge-repo/claude_session.py` is the working
     reference for the exact flags).
  2. Spawn via `StdioTransport::spawn(binary, &args, &env)`, set `cwd` to the
     session working dir (transport currently inherits cwd — add a `spawn_in`
     variant or set it on the `Command`; small change to `transport.rs`).
  3. Write the prompt to stdin, close stdin.
  4. Read lines in a loop; parse each as claude stream-json; translate to
     `AssistantMessageEvent`; push onto an `mpsc` that backs the returned
     `BoxStream`. EOF or a `result` event → `Done`.
- The translation table (claude event → Ocean event) lives here. It's the
  inverse of what the bridge's `_handle_line` already does
  (`telegram-bridge-repo/claude_session.py`): `content_block_delta/text_delta`
  → `TextDelta`; `content_block_start/tool_use` → tool-activity; `result` →
  `Done` with usage + cost pulled from the result event.

### 2. Registration — `ocean-providers/src/lib.rs`

- `ProviderId::Subprocess` variant (enum ~line 29).
- A model alias → selection mapping in `resolve_model_selection()` (~line 463),
  e.g. `claude-cli` → `{ provider: Subprocess, api: "subprocess",
  model: "claude", base_url: "" }`.
- `credential_env_names()` (~line 60): none needed — the spawned `claude` uses
  its own `~/.claude` auth. Document that explicitly.

### 3. Dispatch — `ocean-protocol/src/lib.rs:44`

Add one arm to `stream_simple`:
```rust
"subprocess" => SubprocessProvider::from_env().stream(model, context, options).await,
```

### 4. Config — where the binary path/flags come from

MVP: `SubprocessProvider::from_env()` reads `OCEAN_CLAUDE_BIN` (default:
`claude` on PATH) and a fixed flag set. Later: an `ocean.toml [subprocess.claude]`
section. Do **not** overload `StreamOptions::base_url` for this — add a small
typed config rather than smuggling a path through a URL field.

### 5. Passthrough mode — `ocean-runtime`

When the resolved provider is a subprocess agent, the agent loop must **not**
run its own tools/permissions for that turn (the worker owns them). Two ways:

- **Capability flag (preferred):** add `fn owns_agent_loop(&self) -> bool {
  false }` to the `Provider` trait (default `false`; `SubprocessProvider`
  returns `true`). In `agent_loop.rs` (~line 176), when `owns_agent_loop()`,
  run a single stream-to-`Done` pass and skip the tool-execution / re-prompt
  cycle (`agent_loop.rs:315+`).
- This keeps the daemon's session, transcript, SSE, and `/v1/agent/turns`
  surface identical — only the inner loop short-circuits.

### 6. Process lifecycle

- `kill_on_drop(true)` (already in `StdioTransport`) handles the common case.
- `StreamOptions` carries a cancel token — wire it so cancelling a turn
  (`POST /v1/requests/{id}/cancel`) kills the child. The transport's `close()`
  plus dropping the stream should suffice; verify the child actually dies.
- Worker exits non-zero / emits no `result` → emit `Error { reason, ... }` with
  stderr context, don't hang.

## End-to-end path (unchanged above the provider)

`POST /v1/agent/turns` (`ocean-daemon/src/main.rs`) → resolve model/provider
(`ocean-providers`) → `run_agent_with_history` (`ocean-runtime/src/agent_loop.rs:55`)
→ `stream_simple` dispatch (`ocean-protocol/src/lib.rs:49`) →
`SubprocessProvider::stream`. The daemon, session store, and SSE
(`/v1/agent/events`) don't change.

## Verification

1. **Unit:** feed a captured `claude --output-format stream-json` transcript
   (a `.jsonl` from `~/.claude/projects/...`) through the translator; assert the
   `AssistantMessageEvent` sequence (Start → deltas → Done with usage).
2. **Integration:** spawn a real `claude` with a trivial prompt ("say ok") via
   `SubprocessProvider::stream`, collect the stream, assert `Done` + non-empty
   text. Gated behind an env flag so CI without `claude` auth skips it.
3. **Cancel:** start a long turn, fire the cancel token, assert the child PID is
   gone within ~1s.
4. **End-to-end:** `POST /v1/agent/turns` with `model: "claude-cli"`, confirm
   tokens stream on `/v1/agent/events` and a session `.jsonl` is written by the
   worker.
5. **Bridge swap (the payoff):** point `telegram-bridge-repo` at
   `POST /v1/agent/turns` with `model: claude-cli` instead of spawning its own
   `claude`; confirm identical behaviour. This retires the bridge's
   `claude_session.py` in favour of the daemon owning the worker.

## Scope cuts (ponytail)

- No `ToolActivity` enum variant for MVP — render worker tool calls as text.
  Add when surfaces need to distinguish it.
- No `ocean.toml` config for MVP — env var + defaults. Add when a second
  subprocess agent (codex) needs different flags.
- Codex (`codex exec --json`) is the same shape with a different translation
  table — out of scope here, but the crate is structured so it's a second
  translator + a second alias, nothing architectural.
