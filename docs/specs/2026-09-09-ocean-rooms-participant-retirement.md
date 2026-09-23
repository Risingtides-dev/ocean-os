# Ocean Rooms — S0: participant retirement, aliases, attributed audit lines, ocean-mcp identity

**Status:** implemented 2026-09-09 as the daemon half of the Rooms design direction's slice S0 (`ocean-surface/docs/OCEAN_ROOMS_DESIGN_DIRECTION.md`, PR ocean-surface #220). Surface slices S1+ consume what this document specifies.
**Date:** 2026-09-09
**Authorizing documents:** the accepted Phase 1 manifest (operator lane, decision replay) and the Phase 2 manifest (room-wide decision namespace); the design direction names the need.

## 1. Problem

Before the surface learned who a person is, it minted two kinds of placeholder
human: the single-operator constant `surface-operator` (shown as "Operator")
and random `web-<16 hex>` ids per anonymous browser session. Those rows own
rooms (`room_local_roles`), own agents (`room_agent_owners`), are frozen into
agent bindings as `owner_member_id`, and authored messages. On the `campaigns`
room one person appears three times. The ledger is append-only, so the fix
cannot be a rewrite.

## 2. Ruling

Retirement is a **merge**, not a deletion, and only placeholders can be merged.

- `POST /v1/rooms/persistent/{key}/participants/{id}/retire
  {decision_id, successor_id}` — operator lane (`X-Ocean-Operator`),
  replay-safe in the room-wide decision namespace shared with agent bindings,
  profiles, and folder grants.
- `{id}` must be `surface-operator` or match `^web-[0-9a-f]{16}$`. Any other
  id — a real person, an agent — is 400 `participant_not_retirable`. This is
  the guard against the route becoming an identity-takeover primitive.
- `successor_id` must be a live Human participant of the room (409
  `successor_not_human` otherwise); a placeholder or the same id is 400
  `invalid_successor_id`. A `{id}` that is not on the roster is 404
  `participant_not_found`.
- One IMMEDIATE transaction: move the Local room owner role (the partial
  unique index keeps one owner; the placeholder's role row is deleted and the
  successor is promoted or inserted), point every `room_agent_owners.owner_id`
  at the successor, delete the placeholder's roster row, insert
  `room_participant_aliases(from -> to)`, record the decision, append one
  System row `{"type":"room.participant.retired", from, to, owner_moved,
  agents_moved, actor, decision_id}`.
- Bindings keep their frozen `owner_member_id`. Owner and target proofs
  (`prove_owner_and_target`) resolve that id through the alias chain (bounded
  to 8 hops) before comparing to the live owner, so the successor can
  suspend, resume, and re-authorize what the placeholder authorized without
  any ledger rewrite.
- `GET .../inspect`, `GET .../{key}` (room detail), and `GET .../snapshot`
  carry `aliases: [{from, to, retired_at}]` (an empty array when none).

## 3. Attributed audit lines

`room_history_text` — the ONE projection every reader of a System body goes
through (model prompt, room detail, snapshot, summary) — now emits:

| Wire type | Text |
| --- | --- |
| `room.agent.admission` (outcome != refused) | `[room agent admission audit] {agent}` |
| `room.agent.admission` (outcome = refused, reason_code) | `[room agent admission refused: {code}] {agent}` |
| `room.agent.authority` | `[room agent authority audit] {agent}` |
| `room.agent.bootstrap` | `[room agent bootstrap audit] {agent}` |
| `room.agent.output` | `[room agent output audit] {agent}` |
| `room.participant.retired` | `Participant retired: {from} -> {to}` |

The agent MEMBER id is roster-public (it is what people @mention) and rides
after the label; an id that could forge a row (brackets, control characters)
is dropped. Operator principal ids, decision ids, digests, and capability
sets never appear. A refusal whose `reason_code` is not an identifier falls
back to the plain label. The leak tests that previously forbade the package
name in a model prompt now forbid only the private strings.

## 4. ocean-mcp identity

`ocean-mcp` resolves its member id as: `--member` flag, then `member.toml` in
the Ocean config dir (`OCEAN_CONFIG_DIR`, else `~/.config/ocean-rs`), then
`OCEAN_MEMBER_ID`. With none of them, every read works and `ocean_room_post`
/ `ocean_room_join` refuse with a hint naming the file. There is no `$USER`
or "operator" default any more: a bridge that does not know who you are does
not post as anyone. Format:

```toml
member_id = "smaths"
display_name = "John"   # optional
```

The surface's desktop shell reads the same file (design direction §3.2), so a
person is one id from a terminal and from the app.

The daemon publishes the same answer on `GET /v1/identity` (credential-free):
`{ok, member_id: string|null, display_name: string|null, source:
"member.toml"|"env"|"unset"}`, resolved per request from `member.toml` in
its config dir then `OCEAN_MEMBER_ID`. Nothing configured answers
`member_id: null` — never the process user — so a direct host (the desktop
app, the extension) and the proxy's cross-check see exactly what `ocean-mcp`
would post as. A malformed `member_id` is absent, not repaired, and does not
block the env fallback. Note for S1: the design
direction wrote `~/.config/ocean/member.toml`; the daemon's config dir is
`~/.config/ocean-rs`, and that is the path this document fixes.

## 5. Tests

- Store: retiring the placeholder moves the owner role, every agent it owned,
  and the roster row, writes an alias and a content-minimal audit row; a
  second retirement into the same member and a chain (`a -> b -> c`) resolve;
  replay is idempotent; changed content or an id consumed by the
  agent-binding ledger is refused; refusals write nothing.
- Daemon: the route is 503 without the operator; a real person, an agent, and
  an uppercase-hex id are `participant_not_retirable`; a placeholder or self
  successor is `invalid_successor_id`; an agent or unknown successor is
  `successor_not_human`; the real retirement moves owner and agent, replays
  unchanged, and a new decision on the retired id is 404; `inspect`,
  `snapshot`, and room detail carry `aliases`; the binding's frozen owner stays
  `surface-operator` while `owner_eligible` is true and the successor can
  suspend it; the second placeholder retires without owning anything; the
  transcript renders `Participant retired: …` and `[room agent authority
  audit] helper`.
- Audit text: admission allowed/refused forms, forged ids dropped, missing
  ids omitted, the retired line.
- ocean-mcp: strict `member.toml` parsing; flag precedence; without identity
  reads work and writes refuse with the hint and nothing is posted.

## 6. Migration on the campaigns room

After deploying from main: retire `surface-operator` and
`web-18c11f5d551e63f8` into `smaths` under two operator decisions, record the
decision ids in `events.md`, and confirm `GET .../inspect` shows
`owner.member_id == "smaths"` and two aliases.
