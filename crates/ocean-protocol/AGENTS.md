# ocean-protocol — Provider Wire Protocol

## Purpose

This crate owns the multi-provider LLM wire protocol layer for Anthropic, OpenAI, Gemini, and OpenAI-compatible providers.

## Ownership

- **Scope:** `crates/ocean-protocol/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Primary responsibilities:** provider request/response translation, streaming protocol handling, provider-specific wire compatibility

## Local Contracts

- Shared `ThinkingLevel::Max` serializes as `max`; current GPT-6 Responses and Claude 5.5/Fable 5.1 encode it exactly. Legacy and other-provider encoders retain their documented ceiling, and the public effort catalog advertises only effective controls.

- Keep provider-specific behavior isolated behind protocol abstractions.
- Do not leak provider quirks into shared `ocean-core` types unless the shared contract intentionally changes.
- Treat streaming event shape changes as compatibility-sensitive.
- Codex OAuth requests using the `codex_cli_rs` originator must carry a current
  `version` header; ChatGPT version-gates newly released Codex models.
- Current Claude Opus/Sonnet 5.5 and Fable 5.1 use adaptive thinking and `output_config.effort`; Sonnet 5.5 Off uses `between_tools` with low effort and no block binding only without replayable Anthropic-signed thinking history; foreign opaque markers do not constrain the mode. Sonnet 5.5 encoding drops thinking from Opus 5/5.5, Fable, and Mythos families that its documented compatibility contract cannot read; supported older Claude history remains replayable. With replayable signed history, retain adaptive low effort and binding controls so per-turn changes and edited prefixes remain valid; omit sampling overrides and manual budgets. Client-side history shaping uses `block_binding.prefix_mismatch_behavior=drop_block` so edited prefixes lose invalid thinking rather than failing the turn; requests carrying this field merge `thinking-binding-controls-2026-08-01` into the beta header alongside OAuth and caller-supplied betas.
- Gemini 3 groups consecutive parallel tool results into one user content with ordered function-response parts; normal user/model messages delimit groups. Gemini 3 omits sampling overrides and carries tool-result images inside the matching `FunctionResponse.parts`; legacy Gemini retains separate image content. Gemini 3 uses `thinkingLevel`, never a legacy budget. Off/Minimal maps to `low` on 3.8 Flash and 3.1 Pro; 3.5 Flash-Lite accepts `minimal`. Persist exact ordered response parts with their `thoughtSignature` metadata behind the private `google-parts:` marker; replay only on the same Google model. Thought summaries and opaque markers never enter visible text or another provider's request.
- API-key OpenAI Responses shares the Codex Responses encoder/decoder but sends only ordinary bearer/content headers to api.openai.com. Codex originator, account, version, and session headers remain subscription-only. API-key GPT-6 Luna Off uses supported `none`; other GPT-6 and subscription routes retain a low floor. Encrypted reasoning replay requires the same provider/model, so subscription artifacts cannot cross into API-key turns.
- MiniMax M3 and M3.1 Flash Preview retain same-model `reasoning_content` across tool rounds. GLM 5.3/Flash always sends enabled thinking when a level is requested; Off means the lowest supported effort, never an unsupported disable request.
- Anthropic extended-thinking requests must keep `budget_tokens` at least 1024
  and strictly below `max_tokens`; preserve explicit output caps by clamping the
  thinking budget rather than raising the cap.
- Kimi Coding `k3` uses the non-adaptive Anthropic encoder: Off omits thinking; Minimal/Low/Medium/High/Max select 1024/2048/8192/16384/24576-token budgets before output-cap clamping. Raw Moonshot `kimi-k3` has a separate Max-only effort contract.
- Anthropic assistant thinking history is replayable only with a non-empty
  provider signature. Drop unsigned cross-provider reasoning at wire encoding;
  never convert it into visible text or reject the shared persisted schema.
- Anthropic replay must omit empty text blocks, including empty tool-result text; an empty tool result omits optional `content`. Rolling cache breakpoints skip thinking/redacted-thinking blocks and bind to the last cacheable block.
- Codex OAuth turns with a bound Ocean session must use that stable session id
  for both `prompt_cache_key` and the HTTP `session_id`; a fresh UUID is only
  valid for ad-hoc provider calls with no session.
- Codex (Responses API, `store: false`) must round-trip `reasoning` output
  items: always request `include: ["reasoning.encrypted_content"]`, capture each
  item verbatim (minus `status`) behind the `codex-item:` marker in
  `thinking_signature`, and replay it in stream order immediately before its
  paired item. A trailing reasoning item with no follower is dropped (the API
  400s otherwise), and other providers must never forward marker-signed
  thinking. Dropping these items is the documented trigger for gpt-5.x
  degeneration into malformed tool calls (harmony `to=functions.*` leakage,
  token salad in argument strings). Malformed tool-call argument JSON still
  fails open to `{}` but must WARN with the raw payload.
- Dynamic tool declarations are ordered request-only Kimi K3 epochs. Only the
  exact `openai-completions`/`kimi`/`kimi-k3` route on Moonshot's official endpoint
  may encode nonempty groups, as content-less `role: system` messages at validated
  transcript indices; unsupported routes, duplicate tools, unsorted groups, and
  the 16-tool/512-KiB overflow fail closed. Historical epochs remain separate.
  Final synthesis retains them for history validity while emitting
  `tool_choice: "none"`. K3 omits fixed temperature, uses
  `max_completion_tokens`, maps enabled reasoning to `max`, and replays
  `reasoning_content` only from same-provider `kimi-k3` assistant history.
- Retries are operator-visible, not log-only. `with_retry_observed` notifies an
  optional `StreamOptions::retry_observer` before each backoff sleep; providers
  must pass `options.retry_observer` through so a reconnect can reach a surface
  instead of leaving clients on a silent spinner. `with_retry` remains the
  log-only form for call sites with no user-facing stream. The reported `reason`
  is a fixed `RetryReason` vocabulary classified from the error type — never
  provider body text, which is attacker-influenced, unbounded, and fans out to
  every connected client.
- Tool `parameters` are carried verbatim by every provider encoder. The minimizer
  M2 plain-object `command`/`argv` Bash schema fixture
  (`tests/fixtures/bash_argv_tool_schema.json`) is pinned against Anthropic,
  OpenAI, Codex, and Gemini request bodies and must match `ocean-runtime`'s
  `argv_mode_parameters()`; do not add provider-specific schema rewriting.
- `OCEAN_PROMPT_CAPTURE_DIR` is an opt-in local diagnostics path: capture the
  complete serialized JSON body only (never request headers or endpoint URLs),
  warn-and-continue on capture failures, and retain owner-only permissions
  because request bodies contain private instructions, transcript, and tool data.



## Work Guidance

- Add focused tests or fixtures when changing provider serialization/deserialization.
- Prefer explicit errors for unsupported provider features.
- Coordinate model-routing assumptions with `ocean-providers` when relevant.

## Verification

- `cargo test -p ocean-protocol`
- `cargo check --workspace`

## Child devlog Index

No child boundaries defined within `ocean-protocol/` at this time.
