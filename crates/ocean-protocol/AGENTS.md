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

## Work Guidance

- Add focused tests or fixtures when changing provider serialization/deserialization.
- Prefer explicit errors for unsupported provider features.
- Coordinate model-routing assumptions with `ocean-providers` when relevant.

## Verification

- `cargo test -p ocean-protocol`
- `cargo check --workspace`

## Child devlog Index

No child boundaries defined within `ocean-protocol/` at this time.
