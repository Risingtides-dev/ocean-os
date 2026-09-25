# Ocean web identity and node linking — program

**Date:** 2026-09-25
**Status:** Program approved by operator on 2026-09-25
**Type:** Cross-repository program direction with milestone gates
**Scope:** ocean-surface (`crates/ocean-surface-proxy`, Surface UI), ocean-os (daemon, `ocean-oauth`, onboarding), ocean-bedrock (room hub)
**Implementation authority:** Authorizes Milestone 1 in full. Milestones 2 and 3 each require a short route/contract manifest — appended to this document or written separately — accepted before any daemon public route or hub wire contract changes. Milestone 4 requires only the contracts M2 and the existing Rooms manifests already fix. See §7.

## 1. Purpose

A coworker should be able to open `https://ocean.agentsworld.org`, sign in
with the identity the team already uses (GitHub, gated on the
`KINGMAKER-SYSTEMS` organization), link their own Ocean node to that login,
see and manage the provider coding-plan logins on that node (Claude and Codex
subscriptions), and join and collaborate in Rooms — all as one person, with one
member id, without the operator hand-editing files on their behalf.

The program keeps the central Rooms rule from the accepted
[`Gate 0 decisions and threat model`](2026-08-17-ocean-rooms-gate0-decisions-and-threat-model.md):

> A room can coordinate authority, but every resource-owning Ocean node remains
> the final authority for its files, processes, credentials, and side effects.

The web login is an identity front door, not a new custody point. Provider
credentials stay on the member's node; the proxy and the coordinator see
status, never tokens.

## 2. Current state (facts, 2026-09-25)

- **Proxy login.** `ocean-surface-proxy` (served via `cloudflared` to `:8790`
  on the operator's Mac) already supports multiple users from a hand-edited
  `0600` `~/.config/ocean-surface/users.json` (username/password). Each user
  carries `devices`: daemon URLs restricted to the tailnet (`100.64.0.0/10`),
  added via `ops/add-device.sh`, with per-device observer-token and operator-key
  file paths. The UI picks a device through `/api/devices` and
  `/api/devices/select`. Sessions are a stateless derived cookie.
- **Room identity.** Rooms federate through Bedrock. Each daemon publishes its
  human through `GET /v1/identity`, read per request from
  `~/.config/ocean-rs/member.toml` (then `OCEAN_MEMBER_ID`, never the process
  user). `ops/onboard-teammate.sh` writes `member.toml`; see
  [`../TEAM_ONBOARDING.md`](../TEAM_ONBOARDING.md).
- **Provider login.** Exists only in the TUI (`/login`) through the
  `ocean-oauth` crate ([`../../crates/ocean-oauth/AGENTS.md`](../../crates/ocean-oauth/AGENTS.md)):
  Claude PKCE with a localhost `54545` callback (ephemeral fallback), Codex
  pinned to `http://localhost:1455/auth/callback`. Blocks land in the node's
  `~/.config/ocean-rs/auth.json`. The daemon has no HTTP route for provider
  login or provider status.
- **Onboarding friction.** Adding a coworker today means the operator edits
  `users.json`, runs `add-device.sh`, and copies token paths. There is no
  self-serve path and no link between a web login and a room member id beyond
  matching strings by hand.

## 3. Invariants

These hold for every milestone. A milestone manifest may tighten them, never
weaken them.

1. **The proxy never holds model credentials.** No provider access token,
   refresh token, or API key is stored in, logged by, or forwarded through
   `ocean-surface-proxy`, including transiently in responses. Provider status
   crosses the proxy; provider secrets do not.
2. **The coordinator never holds model credentials** (Gate 0 Decision 11). The
   hub records identities, device public keys, and safe projections only.
3. **No identity by address** (Gate 0 invariant 4). A tailnet IP, MagicDNS
   name, GitHub display name, or daemon URL is never sufficient identity. A
   linked node is identified by its Ocean device key (Gate 0 Decision 2), and a
   human by the GitHub-mapped member id.
4. **The device picker never exposes daemon URLs** to the browser. The UI sees
   opaque device ids and display names; the proxy resolves URLs server side.
5. **One human per node** (Gate 0 Decision 3). A node links under exactly one
   member; the node owner can unlink it locally without hub approval.
6. **Tailnet stays the transport boundary** (Gate 0 Decision 1). The proxy and
   hub reach daemons only over the tailnet. No public inbound daemon exposure.
7. **Operator-class authority stays server side.** Anything that mutates a
   node (provider login/logout, room operator actions) uses the room operator
   key class, injected by the proxy only for the device's owner, and is never
   handed to the browser.
8. **Truthful state** (Gate 0 invariant 12). Unlinked, offline, signed-out,
   expired, and unsupported are distinct states in routes and UI.

## 4. Milestones

Ship in order. Each is independently useful and leaves the system consistent
if the next never lands.

### M1 — GitHub sign-in on the proxy (ocean-surface)

**Outcome:** "Continue with GitHub" on the login page signs a coworker in to
their existing roster entry.

- GitHub OAuth App (authorization-code) flow on the proxy. Scope is the minimum
  that can read the caller's org membership (`read:org`).
- Admission requires **active** membership of a configured organization
  (`KINGMAKER-SYSTEMS`), checked against GitHub at sign-in. Pending invites and
  non-members are refused with a distinct message.
- The authenticated GitHub login maps to a roster entry through a new optional
  `github` field in `users.json`. No roster match is a refusal, not an
  auto-provisioned account (auto-provisioning arrives with M2).
- Session: the same stateless derived session cookie as password login, so the
  rest of the proxy is unchanged. The GitHub access token is used for the
  membership check and then discarded; it is not stored in the cookie or on
  disk.
- OAuth `state` is random, single-use, bound to a short-lived `SameSite=Lax`,
  `HttpOnly`, `Secure` state cookie, and compared in constant time on callback.
  `Lax` is required because the callback is a cross-site top-level navigation
  from github.com.
- The callback does not set the session and redirect in one cross-site hop: it
  answers a same-origin page that meta-refreshes to the app, so the
  `SameSite=Strict` session cookie is present on the first real request.
- Password login stays as a fallback and for accounts without a `github` field.
- No provider credentials enter the proxy (invariant 1).

**Done when:** a `KINGMAKER-SYSTEMS` member with a `github` roster entry signs
in on `ocean.agentsworld.org` through GitHub and lands on their device; a
non-member and a forged/replayed `state` are refused; proxy tests cover state
mismatch, missing state cookie, non-member, pending member, and unmapped login.

### M2 — Self-serve node linking (ocean-os, ocean-surface, hub)

**Outcome:** a coworker runs one command and approves a code in the browser;
their node appears in their device picker. Replaces hand-editing `users.json`
and `ops/add-device.sh`.

- `ocean link` (invoked by `ops/onboard-teammate.sh`) starts a pairing. The
  daemon generates, or reuses, an Ocean device key held under Gate 0
  Decision 2 (platform credential store where available; the private key never
  leaves the node).
- The node requests a short, expiring, single-use pairing code from the hub and
  shows it. The user approves that code while signed in (M1) on
  `ocean.agentsworld.org`. Approval is an explicit action on the web side;
  displaying the code on the node is the node-side operator action.
- On approval the hub records `{member id (the GitHub-mapped member), device
  name, tailnet daemon URL, device public key}`, and the node writes
  `member.toml` so `GET /v1/identity` agrees with the web identity. The proxy
  derives its device list from these records instead of hand-edited entries.
- The tailnet daemon URL is routing data, not identity (invariant 3); requests
  to the node must still prove the device key once Phase 3 node identity lands.
- An **outbound relay transport** — the node dialing out to the hub so it works
  off-tailnet — is explicitly deferred. It would put transcripts and tool
  traffic through the hub and needs its own Gate-0-style decision.

**Gate:** a route/contract manifest fixing the pairing endpoints, code format
and lifetime, device-key algorithm and storage (with the Gate 0 cryptographic
review), `member.toml` write semantics, unlink/revoke, and how the proxy
consumes the hub's device records.

### M3 — Coding plans over HTTP (ocean-os daemon, Surface)

**Outcome:** a "Coding plans" panel shows, for the selected node, whether
Claude and Codex subscriptions are signed in, and lets the node's owner sign in
or out.

Proposed daemon routes (final shape fixed by the M3 manifest):

- `GET /v1/auth/providers` — status only: `provider`, `signed_in`, `kind`
  (`oauth` | `api_key`), plan/account label if known, `expires_at`. Never
  tokens, refresh tokens, or key material, not even redacted prefixes.
- `POST /v1/auth/providers/{provider}/login` — begin: returns an authorize URL
  and an attempt id. The daemon binds the localhost callback exactly as
  `ocean-oauth` does (Claude `54545` with fallback, Codex pinned `1455`) and
  runs `finish()` itself; tokens go straight to `auth.json` with the crate's
  atomic 0600 merge.
- `GET /v1/auth/providers/{provider}/login/{attempt}` — attempt status
  (pending, succeeded, failed, expired).
- `POST /v1/auth/providers/{provider}/logout` — removes that provider's block
  and preserves every other block.

All four are operator-authenticated only (the room operator key class). The
proxy injects that credential server side and only when the signed-in user owns
the selected device (invariant 7).

One-click works when the browser runs on the same machine as the daemon,
because the provider redirects to that machine's localhost. From another
machine or a phone the localhost redirect cannot reach the daemon; Claude needs
a manual code-paste fallback, and Codex's pinned redirect may have none. See
open question 3.

**Gate:** a route/contract manifest fixing the four routes, their auth class,
the status schema, attempt lifetime and concurrency (one live attempt per
provider), the paste fallback if adopted, and the `ocean-daemon` route-count
and ecosystem-contract updates.

### M4 — Rooms in the UI under one identity (ocean-surface, hub)

**Outcome:** invites and joins are clicks in the web UI, and the member id a
room sees is the web login identity from M1/M2 — no separate string to keep in
sync.

Uses existing Rooms routes and the M2 identity records; it adds no daemon
authority. Anything that needs a new daemon or hub route goes through that
route's owning manifest.

### Deferred

- Relay / cross-tailnet transport (Gate 0 Decision 1 reversibility clause).
- Phase 3 remote resources (node identity enforcement on requests, remote
  read-only `stat`/`list`/`read`) — its own manifest, as `../../ROADMAP.md`
  already records.
- Organization or service-owned nodes (Gate 0 Decision 3).

## 5. Threat notes

- **OAuth CSRF / login fixation.** Mitigated by the single-use `state` bound to
  the `Lax` state cookie (M1). A callback without a matching cookie is refused
  and the cookie is cleared either way.
- **Open redirect.** The post-login destination is a fixed same-origin path or
  an allow-listed relative path; never a query-supplied absolute URL.
- **Org membership revocation.** Leaving or being removed from
  `KINGMAKER-SYSTEMS` must end web access. With a stateless cookie, the session
  is invalidated on the next membership check; until then it lives to its
  expiry. Whether to re-check membership periodically, and how often, is open
  question 1.
- **Roster confusion.** A GitHub login maps only through an explicit `github`
  field; usernames are never matched implicitly. GitHub user ids, not logins,
  are the durable key once M2 records exist (logins can be renamed).
- **Pairing-code interception (M2).** Codes are short-lived, single-use, and
  only bind a device key the node already holds; approving a stranger's code
  links their node to your identity, so the approval page shows the device name
  and requires an explicit confirm.
- **Credential exfiltration through status (M3).** The status schema is
  allow-listed fields; tests assert no token-shaped field appears.
- **Cross-user node access.** The proxy injects operator-class credentials only
  for the device owner; a signed-in member cannot drive another member's node's
  provider login.

## 6. Open questions

1. Re-check org membership periodically (e.g. on each session refresh or every
   N hours), or only at sign-in? Periodic checks need a stored GitHub token or
   an org-level app credential; sign-in-only leaves revocation bounded by
   session lifetime.
2. Where do M2 device records live: in the hub (Bedrock, matching Decision 11's
   coordinator role) or in the proxy's own store until the hub contract exists?
3. Remote provider login (M3) from a phone or another machine: adopt Claude's
   manual code-paste flow, and is there any viable remote path for Codex's
   pinned `localhost:1455` redirect, or does the panel say "sign in from the
   node itself"?
4. Should M3 also cover plain API-key entry (`kind: api_key`) from the web, or
   stay OAuth-only with keys set on the node?
5. Session lifetime for GitHub-issued sessions: same as password sessions, or
   shorter given question 1?

## 7. Authorization

- **M1** is authorized in full by this document: proxy-side GitHub OAuth, org
  gating, the `github` roster field, and the cookie behavior in §4, landed in
  ocean-surface with tests.
- **M2** and **M3** are directions only. Each requires a short route/contract
  manifest — appended here as §8/§9 or written as a separate spec — accepted
  before any daemon public route, hub wire contract, or on-disk credential
  format changes.
- **M4** proceeds on existing Rooms contracts once M2 identity records exist.
- Nothing here authorizes a relay transport, remote resources, or moving any
  credential off the node.

## 8. M1 amendment — match on the GitHub account id (2026-09-25)

Independent review of the M1 implementation (ocean-surface PR #223) found that
matching a roster entry on the GitHub **login** is unsafe: a login can be
renamed and then registered by a stranger, who would inherit the old owner's
entry and daemon. M1 therefore landed with a numeric `github_id` roster field
(`gh api users/<login> --jq .id`) as the only matched value; the `github`
string is a human label and is never consulted. The derived session token binds
to the id, duplicate ids are refused at load, and the OAuth state cookie uses
the `__Host-` prefix under HTTPS so a sibling `*.agentsworld.org` host cannot
plant it. Org membership is checked at sign-in only; revocation is removing the
roster entry (open question 1 stands).

## 9. M3 route contract (manifest)

**Status:** written and implemented 2026-09-25 under the operator's standing
program mandate ("drive ocean dev indefinitely"); the operator may veto or
amend it before the Surface panel ships.

Routes, all in `crates/ocean-daemon/src/provider_auth.rs`, all
operator-authenticated through `room_operator::OperatorIdentity::authorize`
(the room-authority credential class: `X-Ocean-Operator` header only; a
`Cookie` header or non-allowlisted `Origin`/`Referer` is refused on shape; an
absent key file is 503 `operator_identity_unavailable`, a missing header 503
`operator_credential_missing`, a wrong one 403):

| Route | Success | Errors |
|---|---|---|
| `GET /v1/auth/providers` | 200 `{ok, providers:[…]}` | auth only |
| `POST /v1/auth/providers/{provider}/login` | 202 `{ok, provider, attempt_id, state:"pending", authorize_url, same_machine_required:true}` | 404 `unknown_provider`; 409 `login_unavailable` (callback port would not bind) |
| `GET /v1/auth/providers/{provider}/login/{attempt_id}` | 200 `{ok, provider, attempt_id, state, error?}` | 404 `unknown_provider` / `unknown_attempt` |
| `DELETE /v1/auth/providers/{provider}/login/{attempt_id}` | 200, same body, `state:"cancelled"` when it was pending | 404 as above |
| `POST /v1/auth/providers/{provider}/logout` | 200 `{ok, provider, removed}` | 404 `unknown_provider`; 500 `logout_failed` |

`{provider}` is `claude` or `codex` (the `ocean-oauth` labels). Each status row
is `{provider, label, kind:"oauth", status, source, expires_at_ms, login}`:

- `status` — `signed_in` when the auth-file block has an access or refresh
  token (an expired access token with a refresh token is still `signed_in`:
  the turn-time refresh renews it), `expired` when the access token is past
  expiry and there is no refresh token, `signed_out`, or `unknown` when the
  auth file cannot be read.
- `source` — `auth_file`; or, with no Ocean block, the provider resolver's
  label for a credential the runtime would still use (`env`, `codex_cli`);
  `null` when signed out.
- `login` — the latest attempt projection or `null`.

No field carries a token, refresh token, key, or prefix of one; a test asserts
the serialized status contains neither token value.

Attempts: held in memory only, at most one per provider (the latest), spawned
and registered under one lock so no row exists without its task. Starting a
login, cancelling, and logging out all cancel a pending attempt and wait (≤2 s)
for its task to stop; a task already past its token exchange cannot be stopped,
so when it turns out to have written tokens the attempt is recorded
`succeeded`, never a false `cancelled`, and logout removes the block only after
that write. A superseding start retries its bind for up to 2 s while the old
callback listener — Codex's is fixed at `127.0.0.1:1455` — closes. States are
`pending`, `succeeded`, `failed`, `cancelled`; `failed` carries only a fixed
code (`timeout`, `denied`, `state_mismatch`, `exchange_failed`,
`write_failed`, `login_failed`) because the flow's own text can hold a raw
provider response, attacker-chosen callback `error_description`, or the auth
file's path — the detail goes to the daemon log.

Auth-file concurrency: every in-process writer — login, logout, and the
turn-time refresher — takes `ocean_providers::auth_file_lock()` around its
read-modify-write and uses a unique temp name; the refresher merges its result
into a fresh read and only into a block still carrying the refresh token it
spent. Review of this contract also found the refresher created its temp file
with the default mode, leaving a refreshed `auth.json` world-readable; it now
creates it 0600. A daemon restart
forgets attempts, which is correct because their listeners died with it.
Logout cancels a pending attempt, then removes only that provider's block
through `ocean_oauth::logout` (atomic, 0600, other blocks preserved, a missing
file is not created).

Not in this contract: the remote code-paste fallback (open question 3),
API-key providers (open question 4), and plan/account labels (the tokens do not
carry a plan name the daemon can read without a provider call). The Surface
side — a proxy allowlist that injects the operator key only for the signed-in
owner's selected device and strips `Cookie`/`Origin`/`Referer`, plus the
"Coding plans" panel — ships separately in ocean-surface.
