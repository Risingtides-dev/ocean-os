# ocean-runtime — Agent Loop and Tools

## Purpose

This crate owns the Ocean agent loop and permission-gated tool execution runtime.

## Ownership

- **Scope:** `crates/ocean-runtime/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Primary responsibilities:** agent turn execution, tool dispatch, permission boundaries, runtime event production

## Local Contracts

- A turn that is waiting must say why. Both retry layers — the provider layer's
  request retry (via the `retry_observer` the loop installs on `StreamOptions`)
  and the loop's own clean-round replay — emit `AgentEvent::ProviderRetrying`
  with a `RetryScope`. Silent retrying is what makes a degraded network
  indistinguishable from a hung agent; do not add a third quiet wait.
- Permission gates are mandatory; do not add execution paths that bypass them.
  `PermissionPolicy::should_check` owns the approval-mode boundary: manual may
  broaden checks to all known tools, automatic follows each tool's conservative
  `requires_permission` classification, and only explicit skip-all may suppress
  checks.
- Built-in filesystem/process tools must resolve relative paths and shell commands against the turn's `SessionContext.cwd`, not the daemon process cwd.
- On Unix, `BashTool` owns a fresh process group and Halt/timeout must kill the complete ordinary descendant tree before dropping the direct child handle. Disarm group cleanup only after `child.wait()` completes; deliberately re-sessioned descendants and non-Unix tree termination are outside this contract.
- `LazyBrowser` (legacy-chromium feature only) startup remains mutex-single-flight: one caller probes/launches while peers wait. Bound lock wait, liveness, and launch separately; a liveness timeout preserves the cached handle, while cancellation/launch timeout must cache nothing partial and leave the slot retryable. Default builds compile no Chromium backend: the 19 `browser_*` tools keep identical schemas and return `browser_host_unavailable`; never let a default build launch or probe a browser.
- Tool-using turns must reserve a final synthesis path: do not let repeated tool calls consume the entire turn budget without a user-visible assistant reply.
- Assistant text present in a provider's terminal message must be emitted as `TextDelta` when the provider did not stream text chunks, so SSE clients always render the final reply.
- Emit `TurnCheckpoint` only at provider-valid round boundaries. Its delta must preserve transcript order and, for tool rounds, include the assistant tool-call message followed by every corresponding ordered `ToolResult`; never checkpoint an incomplete batch.
- Runtime events must remain compatible with `ocean-core` event contracts and daemon SSE streaming.
- `ToolExecutionEnd` intentionally sends full live content/details while only the transcript copy is capped (32 KiB text / 256 KiB image). The per-turn event sink remains unbounded; changes to this ownership seam require the finite queue/RSS characterization test and daemon replay review.
- Keep provider concerns outside runtime unless mediated through the protocol/provider layers.
- A tool-call batch runs through the concurrency scheduler (`agent_loop.rs`): consecutive `Concurrency::Shared` (read-only) tools run in one concurrent segment; a `Concurrency::Exclusive` tool (the default) is a full barrier. A new tool defaults to `Exclusive` — only override `concurrency()` to `Shared` when the tool is genuinely side-effect-free. Live `ToolExecutionEnd` + side-effect events must emit in completion order as each segment member finishes; persisted `ToolResult`s must remain in original batch order for provider pairing.
- Blocking await boundaries in the agent loop (provider stream read, tool execution, retry backoff) must race the cancel token pre-yield via a biased `tokio::select!` on `cancelled(config)`. At a tool cancellation boundary, checkpoint the entire assistant tool-call batch with ordered real or conservative results, including not-yet-started barrier calls; completed side effects must never disappear into replayable history.
- Every provider round in a bound agent session must copy `AgentConfig::session_id` into `StreamOptions::session_id`; providers use that stable identity for cross-round prompt caching and request correlation.
- Exact `kimi`/`kimi-k3` turns use bounded dynamic tool discovery: expose only
  the synthetic permission-free `search_tools` globally and return at most eight
  deterministic matches per search. Durable search results persist names only;
  every run reconstructs up to sixteen loaded definitions / 512 KiB from session
  transcript truth, retaining each search epoch as a separate declaration after
  its result and before later calls. If compaction removed the result, anchor the
  recovered tool immediately before its first surviving call (or at transcript
  end when none survives). `search_tools` is reserved and collisions fail closed.
  A real call must have been offered in that exact provider round before ordinary
  permission, cancellation, and execution handling may dispatch it; same-batch
  search results never grant execution authority. Final synthesis retains all
  historical declarations, sets `tool_choice` to `none`, and offers no executable
  tool. Budget-rejected matches are never silently dropped from a `search_tools`
  result: each carries `loaded:false` plus a machine-readable `excluded_reason`
  (`tool_limit` at sixteen loaded tools, `schema_too_large` past the 128-KiB
  per-schema cap, `byte_budget` past the 512-KiB declaration total), and the
  result reports a top-level `excluded_count`. Durable `loaded:false` entries
  are display truth only — reconstruction never loads them and they emit no
  declaration on resume.
- The reproducible history-cost kernel is `examples/history_cost_bench.rs`: run it in release mode from a clean revision with the fixed 10/100/1,000-message × 1/5/20-round matrix. Treat it as trim/serialization/clone scaling evidence, not end-to-end turn latency.
- Hashline-enabled sessions expose both `edit` and `hashline_edit`; every profile retains `write`. Artifact spill is enabled by the daemon for TUI/ACP/CLI/web and disabled for voice; direct callers default off. Code intelligence (the `lsp` tool) is enabled for TUI/ACP/CLI/web and disabled for voice, whose spoken replies cannot carry definitions/references/diagnostics; direct callers default ON so legacy behavior is unchanged. These are the only effective profile gates currently copied into `SessionContext`. Controlled GPT-5.6 Terra benchmarks showed that hiding `edit` changed model exploration behavior and doubled wall time even when the model still selected `hashline_edit`.
- `TodoTool` state is session-scoped in memory for bound sessions so the
  Files-sidebar todo pin and executable tool remain consistent across turns.
  Separate sessions are isolated; unbound/ad-hoc runs receive fresh state; a
  daemon restart remains the durability boundary. The session cache has a soft
  bound of 1,024 recently-used entries: eviction only reclaims **empty** idle
  tools — non-empty sessions are never silently dropped, preventing the TUI
  tray from displaying a projection with no corresponding daemon tool. Todo
  items keep authoritative `text` plus an optional agent-supplied `title` of at
  most 36 terminal cells; list output retains the text, while compact clients
  may render the title.


## Work Guidance

- Prefer small, auditable tool-execution changes.
- Include tests for permission-sensitive behavior.
- Coordinate event shape changes with `ocean-daemon`, `ocean-core`, and `ocean-tui`.

## Verification

- `cargo test -p ocean-runtime runtime_event_queue_retains_full_tool_payload_until_drained`
- `cargo test -p ocean-runtime bash_halt_ -- --test-threads=1`
- `cargo test -p ocean-runtime --features legacy-chromium lazy_browser_ -- --test-threads=1`
- `cargo test -p ocean-browser --features legacy-chromium cancelled_browser_launch_does_not_orphan_spawned_process -- --test-threads=1`
- `cargo test -p ocean-runtime`
- `cargo run --release -p ocean-runtime --example history_cost_bench -- --warmup 5 --samples 30 --output <path>`
- `cargo check --workspace`

## Child devlog Index

No child boundaries defined within `ocean-runtime/` at this time.
