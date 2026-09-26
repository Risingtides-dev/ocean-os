# Rooms member-lane authentication — proposal

**Date:** 2026-09-26
**Status:** PROPOSED — awaiting the operator's yes/no (§7). Nothing here is authorized to build.
**Closes:** the member-lane half of [Rooms DoD](2026-09-01-ocean-rooms-definition-of-done.md) item 3.1 ("identity is not a caller-asserted author_id"; "invite mint and redeem require an operator").
**Fits with:** [web identity program](2026-09-25-ocean-web-identity-and-node-linking-program.md) §10 (M2 linked-surface credential). It reuses `X-Ocean-Link` as a credential and competes with nothing in §10 (see §6).
**Scope:** ocean-os daemon and store, the ocean-surface proxy and UI. No Bedrock change is needed (§5).

## 1. Current trust chain

Line numbers are for `origin/main` at `bd2f9abb` (ocean-os) and `1a79ae1` (ocean-surface `fix/proxy-path-actor-csrf`, PR #230, open).

**Daemon perimeter.** Every request passes `host_guard::refuse_foreign_hosts` (`crates/ocean-daemon/src/host_guard.rs:257`, 421 on a foreign `Host`) and `cross_site_write::refuse_cross_site_writes` (`cross_site_write.rs:128`, 403 on a `Cookie`, or on a present untrusted `Origin`/`Referer`). Both are layered at `main.rs:930-935`. An absent `Origin` passes (`cross_site_write.rs:31-38`). The listener is served without `ConnectInfo` (`main.rs:1477-1486`), so no handler knows its peer address. The room routes are listed at `main.rs:3217-3414`.

**Two lanes, one of them authenticated.**

- *Operator lane.* The `X-Ocean-Operator` header (`room_operator.rs:38`) is compared in constant time against `<config_dir>/operator.key` (`room_operator.rs:140`, `:191`), after the cookie refusal and `check_origin` (`:221`). This lane is used by the six agent-authority routes (`room_agent_authority.rs:631`, `:863`, `:1000`, `:1096`, `:1219`), retirement (`room_retirement.rs:80`), resources (`room_resources.rs:400`, `:512`, `:769`), profile (`room_profile.rs:557`), maintenance (`room_maintenance.rs:817`), and the optional operator path of close (`persistent_rooms.rs:1470-1476`).
- *Member lane.* This lane carries no credential. The actor is whatever the request names:
  - `?actor_id=` for close (`persistent_rooms.rs:1416`, `:1477`), attachment delete (`room_attachments.rs:369`), and every workspace call (`room_workspace_proxy.rs:834`);
  - `?uploader_id=` for attachment upload (`room_attachments.rs:359`);
  - body `author_id` for posts, roster-checked by `classify_local_author` (`persistent_rooms.rs:2605`, `:2714`), and for artifacts (`:2104`, `:2141`);
  - body `invoked_by` for agent invoke (`:2935`);
  - the join body `id` and `owner_id` (`:1889`).
  Two routes read no caller at all, only a target: leave (`DELETE participants/{id}`, `:2401`) and remove (`DELETE members/{id}`, `:3276`). Invite mint (`:3129`), redeem (`:3172`), register-agents (`:3210`), read-cursor and outbox retry take no identity either.

**Federated rooms speak as one human.** A federated room holds a single Bedrock bearer and `local_human_member_id`, an opaque Bedrock UUID (`ocean-store/src/lib.rs:2052`). It is installed at create (`room_federation.rs:1484`) and at redeem (`:1714`). Every federated write uses that bearer:

- a post ignores `author_id` and is authored as `local_human_member_id` (`persistent_rooms.rs:2752`, which leads to `room_federation.rs:1272`);
- remove sends `DELETE` with the bearer (`room_federation.rs:2251`);
- the workspace lane maps any Human roster actor onto `local_human_member_id` (`room_workspace_proxy.rs:785-791`). Its comment says "every browser session on it IS that principal".

The owner proof used by authority is `room_owner_proof` (`room_agent_authority.rs:664`). For a Local room it reads the `room_local_roles` owner, whose eligibility means the owner is still a Human participant (`ocean-store/src/lib.rs:1804`, `:3843`). For a federated room it reads `local_human_member_id` (`room_agent_authority.rs:679`).

**Node identity.** `GET /v1/identity` (`identity.rs:71`) reads `member.toml` and then `OCEAN_MEMBER_ID` (`identity.rs:82`). It is published, but it is not used as an authority input.

| Caller | Reaches the daemon as | How the daemon learns who it is |
|---|---|---|
| `ocean` CLI, TUI | loopback, no credential | Neither calls a room route today (no `/v1/rooms` in `crates/ocean-cli`, `crates/ocean-tui`). |
| `ocean-mcp` | loopback, or a tailnet URL; no credential | Body `author_id` = its own `member.toml` / `OCEAN_MEMBER_ID` (`ocean-mcp/src/bin/ocean_mcp.rs:131`, `:442`), unverified. |
| Surface proxy, single-user / auth-off | loopback or tailnet; operator key on the six authority shapes only | The UI sends `surface-operator` or the adopted `/v1/identity` id (`ocean-surface-ui/src/rooms.rs:109`, `daemon.rs:2732`), unverified. |
| Surface proxy, multi-user (#230) | same | Session cookie → `session_user` (`ocean-surface-proxy/src/main.rs:1381`) → every actor field must equal the username (`:3241`, `:3295`, 403 `actor_mismatch`). The owner gate for the six authority shapes (`:3389`) looks up `GET {key}/agents` first. The daemon itself still believes the field. |
| Tauri shell | The webview calls the daemon directly (trusted `tauri://localhost` origin, `cors.rs:95`). A Rust forwarder attaches the operator key for the ceremony paths (`ocean-tauri/src/lib.rs:1731`, `:1800`, `:1843`). | Same as single-user. |
| Chrome side panel | direct, `chrome-extension://*` trusted (`cors.rs:94`) | Claimed fields. |
| Bedrock (federation) | Never calls the daemon. The daemon dials out with the room bearer. | Bedrock knows the node as one principal token per room (`ocean-bedrock/db/006_federated_rooms.sql:8`). |

## 2. Threats

| # | Threat | Realistic attacker | Today |
|---|---|---|---|
| T1 | Spoofed leave/remove. `DELETE participants/{owner}` on a Local room flips `owner_present`, and every authority action then fails `owner_eligible`. | A signed-in coworker through the multi-user proxy. The proxy cannot bind a target-only route (proxy AGENTS "Known gap"). | Open. This is a denial of service on agent authority. |
| T2 | Spoofed post, close, artifact, attachment, invoke or workspace call as another roster member. | Anything that reaches the member lane: a coworker through an auth-on proxy that predates #230, a tailnet peer when the daemon binds its tailnet IP, or a local process. | Direct web pages: closed by #510. Through the proxy: closed only by #230's field binding, which the daemon cannot verify. Tailnet peer and local process: open. |
| T3 | Close by any roster human. Close accepts any non-agent roster id, so any member can freeze a shared room. | A signed-in coworker, even with #230 correctly bound to themselves. | Open (policy, not spoofing). |
| T4 | Federated impersonation. In a federated room, a coworker whose device resolves to someone else's node posts, removes, mints invites and drives the workspace as that node's human, because every federated write uses the node's bearer. | A coworker whose `users.json` device (or the device-less fallback, §10 of the program) is the operator's node. | Open, and silent: #230's binding passes because `author_id` equals the coworker's name, and the daemon then ignores it. |
| T5 | Federated owner mapping. `room_owner_proof` names a Bedrock UUID that never equals a roster username, so a multi-user web user gets `not_room_owner` on all six authority actions in a federated room. | None. This is lost function for the legitimate owner. | Open (proxy AGENTS "Consequence for federated rooms"). |
| T6 | Invite mint and redeem, register-agents, outbox retry. Each acts on Bedrock or on the node with the node's authority and checks no caller. | A coworker through the proxy, or a tailnet peer. | Open. DoD 3.1 names mint and redeem. |
| T7 | Check-then-forward owner gate. The proxy's owner lookup and the forwarded mutation are two requests. | None today, because the owner is write-once. Any future owner transfer on a browser route would make it one. | Latent. |
| T8 | Anonymous tailnet member-lane writes when the daemon binds a non-loopback address. | Any peer on the shared tailnet. | Open. §10(2) of the program proposes the daemon-wide fix. |
| T9 | Cross-site page. | A web page the operator visits. | Closed by #510, except a write laundered through an auth-off proxy, which #230's auth-off gate closes. |

Two items are out of scope and stated so that nobody assumes otherwise:

- **A same-user local process.** It can read `operator.key` and drive `/v1/turns`, so no room-level check can stop it.
- **Read authorization.** Any principal that reaches the node reads every room the node hosts.

## 3. Recommended design: a daemon-verified member principal

### 3.1 Principals

A new module, `room_principal.rs`, adds one extractor, `MemberPrincipal`, that runs on every `/v1/rooms/persistent/*` request. It reads only headers and the peer address. It never reads the query or the body.

| Principal | Proven by | Acts as | Override |
|---|---|---|---|
| `Operator` | valid `X-Ocean-Operator`, no acting header | the node human | operator |
| `NodeHuman` | loopback peer, no credential | the node human | none |
| `Delegated { member, via }` | valid `X-Ocean-Operator` (`via = operator:<fp>`) or `X-Ocean-Link` (`via = link:<id>`), plus `X-Ocean-Acting-Member: <member>` | `member` | none |
| `Linked { member }` | valid `X-Ocean-Link`, no acting header (M2) | the link's `member_id` | none |
| *(anonymous remote)* | non-loopback peer, no credential | compat: the legacy claimed field; enforce: 401 `member_principal_required` | — |

The **node human** is the current `identity::resolve` member id (`member.toml`, then `OCEAN_MEMBER_ID`). If it is unset, a route that needs an actor answers 409 `node_identity_unset` with the `member.toml` hint. This is the same rule `ocean-mcp` already follows.

### 3.2 Headers and verification order

1. The `#510` perimeter runs unchanged.
2. `X-Ocean-Link: <link_id>.<secret>` is the M2 credential. A present but invalid value is 403 and is never downgraded. Before M2, the header is refused with 400 `link_credential_unsupported`.
3. `X-Ocean-Operator` is verified through `OperatorIdentity::authorize` exactly as today: cookie and origin shape checks first, then 503 if the key is absent and 403 if it is wrong. A present credential never falls through to a weaker principal. This is the rule `room_close` already follows (`persistent_rooms.rs:1466-1476`).
4. Otherwise, a loopback peer (`127.0.0.0/8`, `::1`, read from `ConnectInfo<SocketAddr>`, which `main.rs` starts serving) is `NodeHuman`.
5. Otherwise the caller is anonymous remote.

`X-Ocean-Acting-Member` must appear once (400 `duplicate_acting_member`) and must satisfy `bounded_member_id`. It is honored only next to a verified credential; without one the request gets 400 `acting_member_requires_credential`, so it is never ignored. With a link, the acting member must equal the link's `member_id` (403 `acting_member_not_delegable`). This keeps §10's rule that a link only ever attaches a node to its own person. The header is pinned by that pairing and is not signed. An HMAC keyed by the credential that travels in the same request proves nothing: an attacker holding the credential can forge the signature, and one without the credential cannot send an accepted request at all. The proxy strips any client-sent `X-Ocean-Acting-Member`, `X-Ocean-Link` and `X-Ocean-Operator` before stamping its own.

**An acting member demotes the request.** `Delegated` never holds operator authority, even when `via` is the operator key. The operator-only routes (retire, resources, profile, maintenance) refuse it with 403 `operator_route_not_delegable`. The six agent-authority routes admit it only when the member is the room owner under §3.4. That moves the proxy's owner gate into the daemon, inside the store lock, which closes T7.

### 3.3 Deriving the actor

Each principal has an **effective actor**. The legacy fields (`actor_id`, `uploader_id`, `author_id`, `invoked_by`, `requested_by`, `owner_member_id`, `owner_id`, and a Human join's `id`) change from *source* to *must-match*:

- **Absent:** the daemon uses the effective actor.
- **Equal to the effective actor:** admitted. This is what every current client sends.
- **Different:**
  - For `Delegated` and `Linked`, the request is refused with 403 `actor_mismatch`, the code the proxy already uses.
  - For `NodeHuman` and `Operator`:
    - in compat, the request is admitted as the named actor, counted, and logged;
    - in enforce, it is refused with 403 `actor_mismatch`.
  - Two exceptions for `NodeHuman` and `Operator`: a join may name another Human (the node human adding someone to a room on their own node), and `owner_id` on an agent join may name the room owner.

The roster checks that exist today (`classify_local_author`, `gate_workspace_call`, the forged-kind gates) still run on the derived actor.

### 3.4 Leave, remove, close

The **effective room owner** of a Local room is the `room_local_roles` owner. When no owner row exists, it is the node human, because the room lives on their node.

- **Leave `DELETE participants/{id}`:** admitted when the target is the actor (self), when the actor is the effective room owner, when the actor owns the target agent (`room_agent_owners`), or when the principal is `Operator`. Otherwise 403 `not_self_or_room_owner`. The room owner may not remove themselves from a room whose owner role is recorded, which matches Bedrock's "rooms cannot be orphaned" (`ocean-bedrock/src/rooms.mjs:542`). That case is 409 `owner_cannot_leave`; ownership moves only through the operator retirement lane. The `ParticipantLeft` marker names the remover when the remover is not the target.
- **Close:** admitted for the effective room owner or `Operator`. Other roster members get 403 `not_room_owner`. This closes T3.
- **Remove `DELETE members/{id}` (federated):** the actor must be the node human (§3.5). Bedrock then enforces "self, or owner" and rejects owner self-leave. The daemon does not duplicate that rule.

### 3.5 Federated rooms: one node human, pinned

A federated credential stands for this node's human (Gate 0 Decision 3, one human per node). The mapping is:

> principal ↔ `local_human_member_id` **iff** the principal's actor equals the room's pinned node human.

- **Storage.** `room_federation` gains `local_principal_id TEXT NULL`: the node human at the moment the credential was installed (create at `room_federation.rs:1484`, redeem at `:1714`). The migration is additive. At startup, `NULL` rows are backfilled once from the current node human if one is set, with one log line per room. If the node human is unset, the row stays `NULL`. `NodeHuman` and `Operator` keep working, but `Delegated` and `Linked` principals get 403 `federated_principal_unpinned`. Pinning means that relinking a node to another person (M2) does not hand that person the previous person's Bedrock memberships through the web.
- **Every federated write requires the pinned node human.** That covers post, remove, invite mint, register-agents, read-cursor, outbox retry, workspace commands and workspace reads. Any other actor gets 403 `not_node_human` and nothing is enqueued. This closes T4 and T6.
- **Owner proof.** `room_owner_proof` for a federated room becomes: the actor equals `local_principal_id`, and `local_human_member_id` is an active Bedrock `owner`. This closes T5. The same web user who owns the node now passes authority in their own federated rooms.
- **Redeem and mint on the node.** Invite mint and redeem are node-human routes. They admit `Operator`, `NodeHuman`, and a `Linked` or `Delegated` principal whose member is the node human. This is how "require an operator" in DoD 3.1 is met in substance: the caller must be the node's own person, proven by credential or by the local machine boundary.

### 3.6 `room-wire.json`

The version goes from `1` to `2`, with two additions. `room_wire_contract_matches_the_daemon` holds both to the router.

```json
"member_auth": {
  "acting_member_header": "x-ocean-acting-member",
  "credential_headers": ["x-ocean-operator", "x-ocean-link"],
  "refusals": {
    "member_principal_required": 401,
    "acting_member_requires_credential": 400,
    "duplicate_acting_member": 400,
    "acting_member_not_delegable": 403,
    "actor_mismatch": 403,
    "not_self_or_room_owner": 403,
    "not_room_owner": 403,
    "not_node_human": 403,
    "federated_principal_unpinned": 403,
    "operator_route_not_delegable": 403,
    "owner_cannot_leave": 409,
    "node_identity_unset": 409
  }
}
```

The second addition is a new `snapshot_keys` entry, `viewer`: `{member_id, principal: "operator"|"node_human"|"delegated"|"linked", is_room_owner, is_node_human}`, computed for the caller. The UI hides owner-only controls from `viewer` instead of guessing. That is the UI gap the proxy AGENTS records.

### 3.7 Compatibility window

`OCEAN_MEMBER_LANE_AUTH=compat|enforce`. The first daemon slice ships `compat` as the default.

- **In compat:**
  - every rule in §3.4–3.5 applies;
  - a new header is strict;
  - a legacy-field mismatch under `NodeHuman`/`Operator`, and anonymous remote requests, are admitted as they are today;
  - admissions are counted in `ocean_room_member_lane_compat_admissions_total{reason}` and a warn log names the route.
- **The default flips to `enforce`** only after the proxy slice (S3) is deployed and the counter has read zero on the operator's daemon for 7 consecutive days.
- **Clients the flip breaks:** only a remote `ocean-mcp` pointed at another person's daemon, and a pre-S3 multi-user proxy. The fix for `ocean-mcp` is to point at your own node or set `OCEAN_OPERATOR_KEY_FILE`; the fix for the proxy is to deploy S3.

## 4. Rollout

| Slice | Repo | Change | Tests / acceptance |
|---|---|---|---|
| S1 | ocean-os | `room_principal.rs`, `ConnectInfo` serving, §3.2 verification, §3.3 derivation in compat, the metric. No `room-wire` change. | Unit tests for every row of the §3.1 table and each §3.2 refusal. The acting header without a credential is refused, never ignored. A presented bad credential is never downgraded. With `via = operator`, `Delegated` is refused on retirement, resources, profile and maintenance. Loopback and anonymous remote are told apart through `ConnectInfo`. Acceptance: current CLI, MCP, Tauri, single-user and #230 proxy traffic is unchanged, and the counter increments only on mismatches. |
| S2a | ocean-os | §3.4 leave, close and remove rules. The `ParticipantLeft` remover. The daemon-side owner gate on the six authority routes. The `viewer` key and `room-wire` v2. | `a_member_cannot_remove_the_room_owner` (the T1 regression, which proves `owner_present` stays true). `owner_cannot_leave_a_recorded_owner_room`. `close_is_owner_or_operator`. `delegated_non_owner_gets_not_room_owner_on_every_authority_shape`. `room_wire_contract_matches_the_daemon` extended. |
| S2b | ocean-store + ocean-os | The `local_principal_id` column, backfill, and pin-on-install. The §3.5 node-human gate on every federated write. The federated `room_owner_proof`. | `delegated_coworker_cannot_post_as_the_node_human` (T4: nothing enqueued). `node_human_passes_owner_proof_in_its_federated_room` (T5). `unpinned_row_refuses_delegated_but_serves_node_human`. `relinked_node_does_not_inherit_the_pin`. A migration test on a pre-column database. |
| S3 | ocean-surface | Depends on #230 merged. In multi-user mode, stamp `X-Ocean-Acting-Member: <session user>` plus the device's deputy credential on every `/v1/rooms/persistent/*` request, reads included. Refuse with 503 `member_credential_unavailable` rather than forward anonymously when the device has no credential; an anonymous loopback forward would make the coworker the node human. Strip client copies. Vendor `room-wire` v2. Drive owner-only controls from `viewer`. Retire the `GET {key}/agents` owner-lookup gate. | Proxy tests: the header is stamped on every rooms shape, a client-sent copy is stripped, a credential-less device is refused, and single-user and auth-off stamp nothing. UI tests: controls hide for `is_room_owner: false`. Acceptance: two roster users on one daemon; B cannot remove A, close A's room, or post into A's federated room; A can use authority in A's federated room from the web. |
| S4 | ocean-os | Flip the default to `enforce` once §3.7's condition holds. Close DoD 3.1's member-lane half. | Mismatch returns 403 and anonymous remote returns 401 under the default. The DoD 3.1 entry names these tests. |
| S5 | ocean-os + ocean-surface | Only if §10 of the program is accepted: `X-Ocean-Link` verification in `room_principal.rs`, and the proxy sends the link secret instead of the copied operator key. | Link + acting header ≠ link member returns 403. `ocean unlink` refuses that link's next request. |

**S1 note: pre-enforcement evidence already exists (2026-09-26).** Before any slice here is approved, the daemon counts the T4 case without changing it: `ocean_room_federated_actor_substituted_total{route}` (routes `post`, `workspace_read`, `workspace_command`; mirrored on `/health` as `rooms.federated_actor_substituted`). It increments when a federated post is enqueued, or a gated non-Agent workspace call is forwarded, and the caller's trimmed, non-empty `author_id` / `?actor_id=` is neither the credential's `local_human_member_id` nor the node's `/v1/identity` id. One `warn` line per increment names the room key, the route, whether the id is on the local roster and whether a node identity is set; it never logs the ids. Nothing is refused and no response changes. The other §1 federated writes (remove, invite mint and redeem, register-agents, read-cursor, outbox retry) read no caller identity at all, their bodies are `deny_unknown_fields`, so there is nothing to compare without adding the §3.1 principal; invoke checks `invoked_by` against the federated roster rather than substituting it. This counter is not §3.7's `ocean_room_member_lane_compat_admissions_total`, which S1 still adds; it is the number to read before answering §7 question 4.

Bedrock needs no slice. Its `removeMember` already enforces self-or-owner and rejects owner self-leave, and one node maps to one principal token per room.

## 5. What this does not change

- Operator-lane semantics for a caller that sends no acting member.
- The `#510` perimeter.
- The CORS trust set: the owed chrome-extension and port narrowing stays a separate compatibility decision.
- Read authorization.
- Bedrock.

A same-user local process remains the node human. Closing that would take a daemon-wide loopback credential, which is not a Rooms decision.

## 6. Relation to M2 §10

This proposal uses §10's credential and does not replace it.

- **Before M2,** the operator key the proxy already holds per device is the deputy credential.
- **After M2,** `X-Ocean-Link` takes that role unchanged (S5). `Linked` is §10's link row, and its `member_id` is the only member a link may assert.
- **The §3.5 pin** reads the same node human that M2 pairing writes into `member.toml`, so a linked node's web identity and its federated owner proof agree without a new mapping table.
- **§3.2's anonymous-remote refusal** is the member-lane subset of §10(2)'s credentialed tailnet bind. If §10 is accepted, §10(2) supersedes it daemon-wide. If §10 is rejected, this subset still stands on its own.

## 7. Operator questions

1. **Principal model.** Should the daemon derive every member-lane actor from a verified principal (the operator key, or `X-Ocean-Link` after M2, optionally with `X-Ocean-Acting-Member`), with caller-supplied `actor_id`/`author_id`-style fields becoming must-match? This is §3.1–3.3. Yes/no.
2. **Loopback is the node human, not the operator.** Should an anonymous loopback caller (ocean-mcp, the Tauri webview, the side panel, a single-user proxy) keep working as the node's human, instead of every local tool having to present the operator key? Yes/no.
3. **Destructive acts.** Should leaving on behalf of someone else, closing, and removing require self (for leave only), the room owner, or the operator, with the recorded owner unable to self-leave (Bedrock parity)? Yes/no.
4. **Federated rooms are the node human's.** Should only the node's pinned human write to a federated room, so that a coworker using someone else's node is refused rather than posting as that node's human? Yes/no.
5. **Window.** Should the daemon stay in compat until S3 is deployed and compat admissions read zero for 7 days, then enforce, at which point anonymous remote member-lane writes get 401? Yes/no.
