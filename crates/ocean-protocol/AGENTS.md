# ocean-protocol — Provider Wire Protocol

## Purpose

This crate owns the multi-provider LLM wire protocol layer for Anthropic, OpenAI, Gemini, and OpenAI-compatible providers.

## Ownership

- **Scope:** `crates/ocean-protocol/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Primary responsibilities:** provider request/response translation, streaming protocol handling, provider-specific wire compatibility

## Local Contracts

- Keep provider-specific behavior isolated behind protocol abstractions.
- Do not leak provider quirks into shared `ocean-core` types unless the shared contract intentionally changes.
- Treat streaming event shape changes as compatibility-sensitive.
- Codex OAuth requests using the `codex_cli_rs` originator must carry a current
  `version` header; ChatGPT version-gates newly released Codex models.
- Anthropic extended-thinking requests must keep `budget_tokens` at least 1024
  and strictly below `max_tokens`; preserve explicit output caps by clamping the
  thinking budget rather than raising the cap.
- Anthropic assistant thinking history is replayable only with a non-empty
  provider signature. Drop unsigned cross-provider reasoning at wire encoding;
  never convert it into visible text or reject the shared persisted schema.
- Codex OAuth turns with a bound Ocean session must use that stable session id
  for both `prompt_cache_key` and the HTTP `session_id`; a fresh UUID is only
  valid for ad-hoc provider calls with no session.
- Dynamic tool declarations are ordered request-only Kimi K3 epochs. Only the
  exact `openai-completions`/`kimi`/`kimi-k3` route on Moonshot's official endpoint
  may encode nonempty groups, as content-less `role: system` messages at validated
  transcript indices; unsupported routes, duplicate tools, unsorted groups, and
  the 16-tool/512-KiB overflow fail closed. Historical epochs remain separate.
  Final synthesis retains them for history validity while emitting
  `tool_choice: "none"`. K3 omits fixed temperature, uses
  `max_completion_tokens`, maps enabled reasoning to `max`, and replays
  `reasoning_content` only from same-provider `kimi-k3` assistant history.
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
