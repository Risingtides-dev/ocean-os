# Ocean Minimizer Command-Capture Runtime Integration Design

- **Date:** 2026-07-16
- **Type:** bounded runtime integration design
- **Status:** Reviewed and accepted design; implementation not started
- **Owner:** Ocean OS
- **Authoring baseline:** `ocean-os` `5b9e23a8a99aacf216cd0c93e66fef5de163e9df`
- **Mechanism baseline:** `ocean-minimizer` M1, merged by PR #298 at `c21f45a8f2052b3a89d4204603c7b5ca98002077`
- **Donor baseline:** Oh My Pi `03c48d073bd4849726cc14750b5aecfa310bdf26` (MIT)

## 1. Decision

Wire `ocean-minimizer` only into model-invoked `ocean-runtime` `bash` tool results, behind a real per-turn harness capability and the existing session artifact store.

The implementation must:

1. accept minimizer identity only from a new, explicitly tokenized `argv` Bash-tool mode; opaque `bash -lc <command>` calls remain ineligible and unchanged;
2. run the existing M1 filter after the direct argv command has completed and its exit code is known;
3. keep the exact pre-minimization Ocean tool-result text in live events, checkpoints, and durable session history;
4. show only the active provider turn a minimized projection plus a turn-pinned `read artifact://<id>` recovery footer;
5. drop that projection and release its artifact pins when the agent run ends, so resumed/future turns see durable raw history rather than stale URIs;
6. fail open to the exact existing tool-result text on every uncertainty or infrastructure failure;
7. preserve permission, cwd, timeout, cancellation, process-group, event, transcript, and scheduling behavior;
8. leave client-owned interactive terminals and every non-`bash` tool unchanged.

This document approves no implementation by itself. M2 requires a separate characterization-first implementation checkpoint and fresh review.

## 2. Current source truth

### 2.1 Execution and capture

`crates/ocean-runtime/src/tools/bash.rs::BashTool` is the only model-callable local shell owner. It:

- remains permission-gated and `Concurrency::Exclusive`;
- executes `bash -lc <command>` in the turn-bound cwd;
- closes stdin;
- creates a Unix process group and kills the ordinary descendant tree on timeout or dropped execution;
- drains stdout and stderr independently, capping each at 2 MiB;
- decodes each captured stream with `String::from_utf8_lossy`;
- renders stdout first, then an optional generated `[stderr]` section, then a generated `[exit N]` suffix.

The existing capture is therefore an Ocean text projection, not a byte-perfect or chronology-preserving process transcript. M2 preserves the exact existing projection; it does not redesign capture, encoding, or stdout/stderr ordering.

### 2.2 Artifact and transcript flow

`crates/ocean-runtime/src/capability.rs::CapabilityRegistry::tools_for_session` currently wraps every merged tool in `SpillingTool` when `SessionContext.artifacts` is true and a bound session store exists.

`SpillingTool`:

- leaves text at or below 24,000 bytes unchanged;
- stores larger text in the bounded session `ArtifactStore`;
- returns a line-bounded head plus `read artifact://<id>`;
- bypasses re-spill for an explicit artifact read.

The post-processed result then reaches both:

- live `AgentEvent::ToolExecutionEnd`; and
- the provider/session transcript, where `agent_loop::cap_tool_content` applies its separate 32 KiB text bound.

M2 must split request projection from authority: live events, checkpoints, and persisted session messages retain the current raw Ocean text, while only the in-memory message clone sent to the provider during the active run receives minimized content. It must not add a second event rail, persist a recovery URI beyond its artifact lifetime, or create a provider-invalid message sequence.

### 2.3 Harness truth

The daemon currently applies exactly two production harness gates through `EffectiveHarnessCapabilities` → `PromptControl` → `SessionContext`: hashline edits and artifact spill. `ocean-minimizer` is not yet a live capability.

M2 adds a third gate only when its runtime branch exists. Direct/legacy callers continue to default off.

### 2.4 Donor lesson, not donor architecture

Pinned OMP:

- refuses unsafe command shapes before whole-buffer filtering;
- returns changed text plus the original capture;
- leaves artifact persistence and footer rendering to the caller;
- treats pipes, compounds, parse errors, unknowns, and oversized captures as passthrough.

Ocean keeps those boundaries but does not import OMP's shell runtime, segmented chains, brush parser, TOML pipelines, user configuration, long-tail filters, or telemetry surface.

## 3. Exact M2 production boundary

### 3.1 Owners

- `ocean-minimizer` continues to own only deterministic filtering of an already-tokenized `Invocation`.
- `ocean-runtime` owns structured argv execution, tool-result envelope handling, turn-pinned artifact recovery, provider-request projection, and decorator composition.
- `ocean-agent` carries one per-turn boolean into `SessionContext`.
- `ocean-daemon` owns the effective surface policy.
- Clients render the ordinary tool result and require no protocol or UI change.

No new crate, daemon route, SDK event, session schema, persisted artifact store, or TUI control is authorized. `AgentToolResult` remains source-compatible and unchanged; provider projection and artifact leases live only in the sealed opaque `ToolExecutionResult`, are never serialized or emitted, and cannot alter live event shapes.

### 3.2 Runtime dependency

`ocean-runtime` may add a path dependency on `ocean-minimizer`. The dependency remains one-way; `ocean-minimizer` must not depend on runtime, artifacts, shell parsing, serde, Tokio, or tracing.

Adding this live consumer moves `ocean-minimizer` into root `default-members`. The package index and non-default-member rationale must be updated in the implementation checkpoint.

### 3.3 Trustworthy invocation source: additive argv mode

Do not classify opaque `bash -lc` source. `shell_words::split`, `split_whitespace`, raw metacharacter scans, basename checks, and best-effort shell parsing cannot prove what Bash actually executes in the presence of startup files, aliases, exported functions, wrappers, or PATH changes.

Add an optional, explicitly tokenized `argv` field to the existing `bash` tool. Use a provider-portable object schema with optional `command`, optional string-array `argv`, and optional `timeout_ms`; do not rely on `oneOf`/`anyOf`. The descriptions state the XOR contract, and runtime validation enforces it:

```json
{
  "argv": ["cargo", "test", "--workspace"],
  "timeout_ms": 120000
}
```

Contract:

1. exactly one of `command` or `argv` is required; both/neither is a validation error before spawn;
2. `command` retains the exact existing `bash -lc` behavior and is never eligible for M2;
3. `argv` must be a non-empty array of strings with a non-empty executable;
4. argv mode executes directly with `Command::new(argv[0]).args(argv[1..])`—no shell, alias, function, expansion, redirection, pipeline, wrapper inference, or re-tokenization;
5. cwd, closed stdin, stdout/stderr pipes, timeout, Unix process group, `kill_on_drop`, capture caps, lossy decoding, markers, and permission behavior are shared with the existing command path;
6. only direct argv whose executable token is exactly one of the six bare M1 names becomes an `Invocation`; slash-containing/arbitrary paths are ineligible, and every other argv remains ordinary unminimized output;
7. M2 adds no extra argv echo: the already-authorized tool arguments continue to appear in existing assistant tool-call/checkpoint and `ToolExecutionStart` paths, but argv is never copied into result metadata, completion events, logs, metrics, or artifacts.

This is an additive tool mode, not a reinterpretation of `command`. Existing callers remain byte-for-byte and execution-semantics compatible. Models may continue using `command`; those calls simply receive no M2 savings. The direct argv token is invocation identity, not a vendor-binary attestation: PATH may select an operator-provided executable, but no shell function/alias can replace it and M1 still requires a recognized output shape.

Provider request fixtures must prove that the revised plain-object tool schema serializes and remains accepted for Anthropic, OpenAI, Codex, and Gemini without provider-specific rewriting.

### 3.4 Private Bash envelope recognition

Do not change public `AgentToolResult.details`, which is currently `Null` and reaches live clients. Both Bash execution modes retain the exact visible result format.

The private decorator knows the submitted argv and may recognize only the exact generated terminal `\n[exit N]` suffix. It parses the final suffix once, passes the preceding current Ocean capture plus the parsed exit code to M1, and restores the suffix in provider content. Exact stdout/stderr cap markers cause passthrough. A command-produced marker collision is allowed to cause a false-negative passthrough.

Timeout, spawn, wait, cancellation, signal, malformed envelope, multiple/non-text blocks, or cap uncertainty never become completed minimization candidates. No exit status is inferred from an arbitrary interior line.

### 3.5 Sealed provider projection and durable raw history

Do not add fields to public `AgentToolResult`; external MCP/plugin providers construct it directly. Instead add a compatibility-safe default method to `AgentTool` that returns a public opaque `ToolExecutionResult` with private fields and crate-private projection constructors. Existing implementors inherit the default plain-result behavior and cannot forge a provider projection.

The agent loop calls this execution method once after permission. The existing artifact wrapper overrides it to return:

- the ordinary `AgentToolResult`; and
- optionally one sealed provider projection plus opaque artifact lease.

The agent loop must:

1. emit ordinary `content` through `ToolExecutionEnd`;
2. finalize and checkpoint ordinary raw `content` in original call order;
3. assign every appended message a monotonic run-local execution ordinal and keep a sidecar aligned through every trim/rebuild operation;
4. bind an override to that exact message ordinal/transcript position, carrying the expected tool-call id only as a pairing assertion—not as identity;
5. immediately before each provider request, clone the provider-valid message sequence and replace only the exact aligned result in that clone;
6. apply existing context trim/cap rules without allowing repeated provider ids (`call_1`, etc.) to retarget historical results;
7. keep artifact leases alive through run success, error, and cancellation cleanup, then drop sidecars/leases when the run returns.

Session persistence therefore contains raw current Ocean tool text and no M2 URI. A daemon restart, artifact eviction after the turn, repeated provider tool-call ids, or later resume cannot strand or misapply a persisted minimized message.

### 3.6 One ordered output-economy wrapper

Do not stack independent minimizer and spiller wrappers with hidden cross-wrapper state. Extend the existing `SpillingTool` (or behavior-preservingly rename it `OutputEconomyTool`) to own the exact post-execution order:

1. call the inner tool exactly once;
2. preserve artifact-read bypass;
3. if any text block exceeds `SPILL_THRESHOLD_BYTES`, perform only today's ordinary spill and return no provider projection;
4. otherwise, for eligible built-in Bash argv only, attempt M1 and a pinned provider projection;
5. return ordinary content plus the sealed optional projection.

The registry still merges/deduplicates first and applies this one artifact-store-owning wrapper to every merged tool when artifacts are enabled. Command minimization is an additional policy on that wrapper and can activate only for the surviving built-in `bash`, a bound session, and the same artifact store.

This exact order guarantees raw live/checkpoint behavior, one artifact path, no minimization of a spill head/footer, and no nested artifact. The wrapper must forward `name`, `label`, `description`, `parameters`, `requires_permission`, and `concurrency` exactly. Characterize and repair the existing missing `SpillingTool::concurrency` forwarding before adding M2 behavior.

### 3.7 Turn-pinned artifact and net-savings rule

A changed M1 result is not automatically used. Apply it only when:

- M1 returns `Disposition::Minimized`;
- capture/envelope recognition is exact and uncapped;
- raw text is at or below the spill threshold;
- saved bytes exceed a fixed footer budget of at least 256 bytes;
- a per-turn pinned-artifact budget can accept the exact raw result;
- final provider text, including exit suffix and footer, is strictly smaller than raw.

Extend the existing in-memory store with a bounded pin/lease operation. M2 pins at most 32 entries and 1 MiB of original output per active run; a pin that would exceed either bound fails open. Pinned entries are skipped by ordinary eviction until their lease drops. This may temporarily exceed ordinary store eviction targets only within those explicit active-run bounds; dropping the final lease immediately reapplies normal eviction.

On success:

1. atomically store and pin the complete original Ocean result text, including generated stderr/exit markers;
2. keep ordinary content unchanged for live events and durable checkpoints;
3. provide the active provider clone with minimized capture, restored `[exit N]`, and:

```text
[output minimized: <input_lines>→<output_lines> lines, <input_bytes>→<output_bytes> bytes · full output: read artifact://<id>]
```

4. retain the lease until the run ends.

If pinning, locking, envelope parsing, net savings, or any invariant fails, return no provider override and create no artifact. The artifact is exact relative to Ocean's pre-M2 UTF-8 text projection, not discarded process bytes. At run end the provider override disappears, raw session history remains, and the artifact may return to normal eviction; no persisted URI survives it.

### 3.8 Harness matrix

Add `command_output_minimization` to:

- `EffectiveHarnessCapabilities`;
- `PromptControl` with a default of `false`;
- `SessionContext` with a default of `false`.

Policy after implementation:

| Profile | Artifact spill | Command minimization |
| --- | ---: | ---: |
| TUI | on | on |
| CLI | on | on |
| room / heartbeat / missing / unknown / unmapped compatibility fallback | on | on |
| ACP | on | on |
| Web | on | on |
| Voice | off | off |

The compatibility-fallback row is an explicit policy decision, not an accidental enum consequence: those turns already receive the CLI artifact capability and M2 gives them the same recoverable model-context reduction. Tests must name `room`, `heartbeat`, missing, unknown, and unmapped tags individually. The runtime additionally requires artifact capability and a bound session store, so minimization can never produce an unrecoverable rewrite. `PromptControl::without_tools()` remains the stronger fail-closed boundary.

This does not classify `surface-tauri` as a new profile; that tag retains the daemon's current CLI compatibility fallback, including the new M2 gate, until cross-repository policy changes.

## 4. Explicit exclusions

M2 does not change:

- TUI Terminal/PTY commands typed by the operator;
- shell execution in build scripts, hooks, plugins, MCP servers, offshore helpers, or external processes;
- existing `command` arguments or `bash -lc` execution; argv mode is additive and shares the existing cwd, environment, permission, timeout, cancellation, process-group, capture, and exit-result contracts;
- stdout/stderr capture order, lossy UTF-8 conversion, or capture caps;
- non-`bash` tools;
- machine-readable, piped, chained, redirected, expanded, compound, unknown, or ambiguous commands;
- M1 filters, command coverage, final line cap, or provenance;
- artifact persistence or restart lifetime; only the explicit 1 MiB active-run pin budget may temporarily defer ordinary in-memory eviction;
- SDK event vocabulary, daemon routes, session storage format, or TUI rendering;
- TOML/user filters, operator settings, metrics, logs containing content, or an environment kill switch.

In particular, M2 must not minimize client-owned interactive terminal output. The TUI PTY is an operator surface, not model context, and has no session-artifact recovery contract.

## 5. Privacy and authority invariants

- Argv eligibility is known from the authorized tool arguments, but execution and filtering still occur only after the existing permission decision; M2 cannot bypass permission checks.
- The wrapper calls the inner tool exactly once.
- No command or output content is logged, labeled in metrics, or copied into metadata.
- Artifact ids remain opaque and session-scoped under the existing store.
- Live events, checkpoints, and session persistence retain ordinary raw tool content; only active-run provider request clones use `provider_content`. This is a bounded request projection, not a second hidden transcript or event rail.
- Artifact reads remain ordinary permission-neutral reads of already-captured session output and retain current windowing/capping behavior; M2 leases guarantee reachability only while their provider URI can exist.
- Unknown/ambiguous outcomes preserve exact pre-M2 live, checkpoint, persisted, and provider bytes and details.

## 6. Characterization-first implementation sequence

### M2a — freeze current behavior

Before production wiring, add tests that freeze:

- exact `BashTool` stdout/stderr/cap/exit envelope;
- timeout and Halt process-tree behavior;
- permission gating and one inner execution;
- current artifact spill threshold, exact raw round-trip, and artifact-read bypass;
- `ToolExecutionEnd` full content versus separately capped transcript content flow;
- decorator forwarding, including the currently missing concurrency forwarding;
- direct `PromptControl` defaults and the existing harness matrix.

M2a may add the decorator-forwarding fix only if reviewed as a required prerequisite; it must not enable minimization or change Bash details.

### M2b — structured argv, provider projection, and decorator

Implement additive argv execution, private envelope recognition, turn-pinned artifacts, sealed provider-only message projection, and the ordered output-economy wrapper. Prove table-driven passthrough for:

- every existing command-string call, including simple-looking commands, pipelines, redirects, substitutions, aliases/functions, PATH/startup-file variations, assignments, wrappers, and parse errors;
- empty or invalid argv, both/neither argument modes, and unsupported executable basenames;
- machine/raw flags already rejected by M1;
- multiple/non-text result blocks;
- truncated captures;
- unsupported or ambiguous output;
- no-change/no-savings and savings below the footer budget;
- missing session/artifact gate/store, exhausted pin budget, poisoned-store fallback, cancellation, and run cleanup;
- raw results over the spill threshold;
- daemon restart/session resume, proving raw history contains no stale M2 URI.

Positive fixtures must cover each M1 program and both success/failure modes where M1 supports them.

### M2c — real profile wiring

Thread the third effective capability through daemon → agent → runtime, with:

- exact matrix tests;
- direct callers still off;
- voice and no-tools turns off;
- web enabled only because artifact recovery is already live there;
- unknown/missing clients retaining CLI fallback;
- no new client request field.

Only M2c changes docs from “standalone/unwired” to “live for conservative model-invoked Bash argv captures.”

## 7. Acceptance tests

The implementation checkpoint is not accepted without all of the following:

1. **Invocation provenance:** existing command strings are never classified; valid argv executes directly and maps exactly to M1; exported functions, aliases, startup files, PATH shadowing, wrappers, and arbitrary basename paths cannot turn shell source into an eligible invocation.
2. **Schema portability:** Anthropic, OpenAI, Codex, and Gemini request fixtures accept the plain-object optional `command`/`argv` schema; runtime XOR validation rejects both/neither without spawn.
3. **Byte identity:** all command-mode, rejected argv, ambiguous, truncated, unsupported, profile-off, direct-caller, and infrastructure-failure cases compare original live/checkpoint/persisted/provider bytes and details exactly.
4. **Artifact exactness and lifetime:** every provider override has one pinned artifact equal to the complete pre-minimization Ocean tool text; same-batch pressure cannot evict it; run end releases it; restart/resume sees raw history with no stale URI.
5. **No double spill:** over-threshold raw output creates only the normal spill artifact; minimized under-threshold provider output is not re-spilled.
6. **Net savings:** the complete minimized provider result plus footer is strictly smaller than raw.
7. **Exit/error truth:** exit suffix and `is_error` semantics remain unchanged; failed command evidence is never converted to success.
8. **Scheduling:** the output wrapper preserves `Concurrency::Shared`/`Exclusive` exactly.
9. **Permission/cancellation:** existing permission, timeout, Halt, and descendant-kill tests remain green.
10. **Event/history/provider:** live `ToolExecutionEnd`, ordered checkpoints, and saved session `ToolResult` contain raw current Ocean text; only the active provider request clone contains the minimized projection plus a currently pinned recovery URI. Repeated `call_1`/`call_2` ids across rounds cannot retarget projections; message order and pairing remain valid through trims.
11. **Carrier compatibility:** existing external `AgentTool` implementations compile unchanged, default to plain execution, and cannot construct a sealed provider projection.
12. **Harness truth:** daemon, `PromptControl`, and `SessionContext` matrices agree; direct callers default off.
13. **Privacy:** existing authorized tool-argument events remain unchanged, while source/tests prove M2 adds no command/output logging, completion metadata echo, or artifact-body exposure.
14. **Provenance:** M1 NOTICE/LICENSE and pinned fixtures remain intact.

Required commands include:

```bash
cargo test -p ocean-minimizer
cargo clippy -p ocean-minimizer --all-targets -- -D warnings
cargo test -p ocean-runtime
cargo test -p ocean-agent
cargo test -p ocean-daemon harness_profile -- --nocapture
cargo check --workspace --tests
cargo xtask ci --compatibility
cargo +1.88.0 xtask ci --msrv
cargo fmt --all -- --check
cargo xtask docs-check
cargo deny check
git diff --check
```

Fresh independent review is mandatory because this changes model-visible command evidence and artifact recovery behavior.

## 8. Rollback and stop rules

Rollback is one harness capability change or a revert of the integration commit; no stored schema or migration exists.

Stop implementation and return to design if any of these becomes necessary:

- shell AST ownership or segmented command execution;
- changing existing command-string execution or stream ordering beyond the explicitly additive direct argv mode;
- minimizing without an exact raw artifact;
- a new public wire event or client request field;
- artifact persistence beyond current in-memory scope or unbounded pinning;
- content-bearing logs/metrics;
- user-defined filters/configuration;
- broadening beyond the built-in model-invoked `bash` tool.

## 9. Follow-on boundary

Walker/search work remains independent. It may reuse runtime cwd/cancellation/tool seams later, but it must not be bundled with M2 or used to justify a broader minimizer parser. The shared walker, typed search engine, and live grep/glob adoption each require their own parity and review gates.
