# ocean-oauth — Provider Login Flows Child Doc

## Purpose

This crate owns browser OAuth 2.0 + PKCE login for provider subscriptions: bind a localhost callback server, hand the caller an authorize URL to open, catch the redirect, exchange the code for tokens, and write the credential block into Ocean's auth file.

## Ownership

- **Scope:** `crates/ocean-oauth/`
- **Parent contracts:** `../AGENTS.md` and `../../AGENTS.md`
- **Primary responsibilities:** authorize-URL construction, PKCE/state generation, localhost callback capture, authorization-code token exchange, atomic auth-file block writes.

## Local Contracts

- Flow constants (endpoints, client ids, scopes, callback ports) mirror OMP's working implementation (`@oh-my-pi/pi-ai` `registry/oauth/{anthropic,openai-codex}.ts`). Do not change them without re-verifying against a working client.
- Claude binds port 54545 with ephemeral fallback; Codex is pinned to `http://localhost:1455/auth/callback` — no fallback, since OpenAI validates the registered redirect URI.
- Written blocks (`claude-code`, `openai-codex`) must stay consumable by `ocean-providers` credential resolution AND `ocean-agent::oauth_refresh` (`type:"oauth"`, `access`, `refresh`, `expires` in epoch ms, `accountId` for Codex).
- This crate performs fresh logins only. Token refresh lives in `ocean-agent::oauth_refresh` / `ocean-protocol::oauth` — never duplicate it here.
- Token endpoints honor the same env overrides as the refresh pass: `OCEAN_OAUTH_ANTHROPIC_TOKEN_URL`, `OCEAN_OAUTH_OPENAI_TOKEN_URL`.
- Auth-file writes are atomic (`.auth.json.tmp-{pid}` + rename, 0600) and must preserve unrelated provider blocks.
- Tests never touch the real `~/.config/ocean-rs/auth.json` and never hit real provider endpoints.

## Work Guidance

- Consumer today: `ocean-tui` `/login` (`Action::Login` → `begin`/`finish`).
- Keep `begin()` non-blocking beyond the port bind; everything slow belongs in `finish()`.

## Verification

- `cargo test -p ocean-oauth`
- `cargo check --workspace` before merge.

## Child devlog Index

- No child boundaries defined within `ocean-oauth/` at this time.
