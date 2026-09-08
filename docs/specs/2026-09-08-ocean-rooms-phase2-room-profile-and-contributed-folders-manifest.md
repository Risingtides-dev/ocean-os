# Ocean Rooms — Phase 2 implementation manifest: room profile and local contributed folders

**Status:** operator-accepted 2026-09-08 with the four §11 rulings recorded below; concurrent implementation and review per the Phase 1 deviation. Stage 2a (`inspect` route) landed through Ocean OS PR #454. Stage 2b (room profile record, `GET`/`PUT .../profile`, credential-slot status on read and at admission) landed through Ocean OS PR #455 and is proven live. Stage 2c (local folder grants, dangerous-root refusal, path confinement, generation-bound status, the §5 cwd rule in the convene path and in `inspect`) is implemented; 2d opens as the §10 gate passes. The operator has authorized this as an ongoing program: work continues through 2b → 2c → 2d without repeated approval prompts, pausing only for a concrete blocker or a design decision this document does not already settle.
**Date:** 2026-09-08
**Phase:** 2 of the Decision 6 capability delivery order
**Authorizing documents:**
[architecture](2026-08-16-ocean-rooms-distributed-workspace-architecture.md) (ratified 2026-08-17) ·
[Gate 0 decisions and threat model](2026-08-17-ocean-rooms-gate0-decisions-and-threat-model.md) (accepted) ·
[Phase 1 manifest](2026-08-25-ocean-rooms-phase1-room-agent-authorization-manifest.md) (accepted, landed 2026-08-31)
**Boundary:** Gate 0 Decision 5 (first resource is a bounded local folder), Decision 6 (stage 2 is local contributed folders), Decision 7 (grants define eligibility, not approval), Decision 13 (audit and content boundary)

## 0. What this manifest is, and what it refuses to be

Decision 6 fixes stage 2 as **local contributed folders**. Phase 1 proved that a
room can hold a pinned, generation-safe authority record for an agent. Phase 2
gives that agent something to be authorized *over*: a folder, and the room-level
declaration that says which repos, tools, and named credentials the room's work
depends on. Both stay on one machine. Nothing in this phase opens a socket to
another Ocean node.

This document adds two records and one read route. It refuses to be:

- a remote-resource protocol (Phase 3);
- a worker admission contract (Phase 4);
- a write, patch, or transfer contract (Phase 5);
- a credential store, sync protocol, or secret transport of any kind; or
- a Surface design for the folder chooser, beyond naming the request shape the
  Surface must send.

### 0.1 Why a profile, and why it must not carry tokens

The operator's question that motivated this phase was how a room connects to
repos, tools, and auth, and how a user's "project file" could carry what an
agent needs to act. The architecture (§12.7) and Gate 0 (Decision 13) already
answer the second half: credentials never leave the executing node and never
enter room context. A profile that physically carries a token is therefore
excluded, not deferred.

What a profile *may* carry is **references**: which repositories, which tool
servers and allowed tools, which named credential slots, which contributed
folders. A node resolves each reference against its own local authority at
execution time and fails closed when a slot is empty. The profile is portable
precisely because it holds intent, not authority. This is the same shape as the
agent package in Phase 1: the package *requests* capabilities; the binding
*grants* them; the node *enforces* them. The profile *declares* dependencies;
the local grant and local auth store *satisfy* them.

### 0.2 Why `inspect` lands first

The operator could not see how Rooms organizes sessions, where agents execute,
or what authority a binding actually resolves to. Every one of those facts
already exists in the daemon, scattered across the room record, the access
projection, the binding table, and a deterministic session-id derivation that
no route exposes. Stage 2a is a single read-only route that projects those
facts together, plus the two Phase 2 records as empty slots, so that each later
stage becomes observable the moment it lands rather than only when the Surface
catches up. It changes no authority and is safe to ship before acceptance.

## 1. Ownership

Both records are **daemon-owned, `ocean-store`-persisted, and never federated**
in Phase 2. The same argument Phase 1 §1 made for the binding applies: a
folder's local root and a credential slot's resolution are node-local facts,
and projecting them to Bedrock would leak paths and policy for no benefit.

| Concern | Owner |
| --- | --- |
| Profile record and validation | `ocean-daemon` (`room_profile.rs`, new) |
| Folder grant record, path confinement, generation | `ocean-daemon` (`room_resources.rs`, new) + `ocean-store` tables |
| Credential slot resolution | `ocean-daemon`, reading `ocean-oauth` auth store and process env; **read-only** |
| Inspect projection | `ocean-daemon` (`room_inspect.rs`, Stage 2a) |
| Folder chooser, grant review, profile editor | `ocean-surface` (out of scope here) |
| Resource-aware tools (`room_list`, `room_read`) | `ocean-runtime` capability seam, gated by binding + grant |

## 2. Records

### 2.1 `room_profile` (new, daemon-owned)

One row per room. Declares what the room's work depends on. Nothing in it is
authority.

```text
RoomProfile
  room_id                 TEXT NOT NULL PRIMARY KEY
  revision                INTEGER NOT NULL           -- bumped on every write
  repos                   JSON NOT NULL              -- [RepoRef]
  tools                   JSON NOT NULL              -- [ToolRef]
  credential_slots        JSON NOT NULL              -- [CredentialSlot]
  default_resource_id     TEXT                       -- room-wide fallback folder for the turn cwd (§5)
  agent_defaults          JSON NOT NULL              -- { agent_member_id: resource_id } per-agent folder (§5, §11.4)
  updated_by              TEXT NOT NULL              -- operator principal id
  updated_at              TIMESTAMPTZ NOT NULL

RepoRef
  alias                   -- room-visible name, e.g. "source"
  remote                  -- https or ssh URL; never a local path
  default_branch
  resource_id             -- optional: the contributed folder that is this repo's checkout

ToolRef
  kind                    -- "mcp" | "plugin" | "builtin"
  name                    -- server or plugin id
  allowed                 -- explicit tool-name allowlist; empty = none

CredentialSlot
  name                    -- e.g. "GH_TOKEN", "VERCEL_TOKEN"
  purpose                 -- one line, display only
  required                -- bool; a required empty slot blocks admission (§6)
  resolvers               -- ordered: ["oauth:github", "env:GH_TOKEN", "keychain:ocean/GH_TOKEN"]
```

Validation is total: an unknown resolver scheme, a `remote` that parses as a
filesystem path, a `resource_id` that does not name a live grant on this node,
or a tool `name` not installed locally is a typed 400 and writes nothing.
Until Stage 2c lands there are no grants, so in 2b any non-null `resource_id`,
`default_resource_id`, or `agent_defaults` entry is refused with
`phase_not_open` rather than silently accepted as a dangling reference.

**Recorded 2b deviation — tool presence is reported, not enforced.** The
"tool `name` not installed locally is a 400" rule above is deferred to Stage
2d. In 2b a `ToolRef` is validated for shape (`kind`, identifier `name`,
canonical `allowed` list) and projected with `installed: "unknown"`; the
write-time refusal lands with the resource-aware tools, when there is one
inventory (MCP servers, plugins, builtins) the daemon can check against
instead of three partial ones.

**Nothing in this record is a secret and nothing in it is federated.** The
`resolvers` list names *where to look*, never *what was found*.

### 2.2 `room_resource_grant` (new, daemon-owned)

One row per contributed folder. This is the local half of the architecture's
`LocalRoomResourceGrant` (§7.3); the projection half (§7.4) is the
`resources[]` entry `inspect` serves.

```text
RoomResourceGrant
  room_id                 TEXT NOT NULL
  resource_id             TEXT NOT NULL              -- opaque, minted by the daemon
  display_name            TEXT NOT NULL              -- room-visible alias
  local_root              TEXT NOT NULL              -- canonical absolute path; NEVER projected
  resource_kind           TEXT NOT NULL              -- "folder" only in Phase 2
  access_mode             TEXT NOT NULL              -- list | read | write | execute (§4)
  authorized_agent_member_ids  JSON NOT NULL         -- empty = no agent may touch it
  expires_at              TIMESTAMPTZ
  generation              INTEGER NOT NULL           -- bumped on every authority change
  status                  TEXT NOT NULL              -- available | suspended | revoked
  granted_by              TEXT NOT NULL              -- operator principal id
  granted_at              TIMESTAMPTZ NOT NULL
  decision_id             TEXT NOT NULL              -- replay key, Phase 1 §3.3 namespace
  request_digest          TEXT NOT NULL

  PRIMARY KEY (room_id, resource_id)
```

`local_root` is the one field with a custody rule: it appears in no route
response, no room message, no audit row, and no model-visible context. The
daemon holds it; everything else sees `resource_id` and `display_name`.

### 2.3 Status semantics

| status | new operation | in-flight | recovery |
| --- | --- | --- | --- |
| `available` | admitted | continues | — |
| `suspended` | **refused** | cancelled at next checkpoint | operator resumes; generation bumps |
| `revoked` | **refused** | cancelled at next checkpoint | terminal; a new grant is a new row |

An expired grant reads as `revoked` at every check and is never silently
renewed.

## 3. The authorizer

Unchanged from Phase 1 §3. Every mutation on either record requires the
`X-Ocean-Operator` header, fails closed with 503 when the operator key is
absent, and consumes a `decision_id` under the room-wide replay namespace.
Reads (`inspect`, `GET .../profile`, `GET .../resources`) stay credential-free,
as Phase 1's binding reads do, because they project no secret and no path.

## 4. Path confinement and dangerous roots

Decision 5 requires the grant flow to reject dangerous roots. Phase 2 fixes
the list:

- the filesystem root;
- the user's home directory itself (subdirectories are fine);
- any path that canonicalizes outside itself through a symlink at the root;
- any root the daemon cannot open as a directory descriptor at grant time; and
- any root already granted to the same room (one grant per canonical root).

Every operation resolves `resource_id → local_root`, joins a normalized
relative path, canonicalizes again, and refuses if the result is not under the
root. The check runs on the node that owns the root, on every call, never once
at grant time.

`access_mode` is a ladder: `execute ⊃ write ⊃ read ⊃ list`. Phase 2 implements
`list` and `read`. `write` and `execute` may be *recorded* so the operator can
express intent, but every write or execute operation is refused with a typed
`phase_not_open` until Phase 4 and 5 manifests are accepted.

## 5. How a turn picks its workspace

Today an authorized room-agent turn uses `Room.workspace_root` as its cwd, and
only if that string still canonicalizes to itself and is a directory. If it
does not, the turn is **refused** with `workspace_unavailable`; there is no
fallback to the daemon launch directory for an authorized turn. That single
optional string is the compatibility surface architecture §18 names. Phase 2
does not remove it. It layers over it:

1. If the profile's `agent_defaults` names a grant for the admitted agent, or
   failing that its `default_resource_id` names one, and that grant is
   `available` and authorizes the agent, the turn's cwd is that grant's
   `local_root`.
2. Otherwise `Room.workspace_root`, exactly as before.
3. Otherwise the turn is refused, exactly as before.

`inspect` reports which rule fired as `execution.cwd_source` so the operator
can see the resolution rather than infer it.

Migration: an existing `workspace_root` is *not* auto-converted into a grant.
Auto-granting would mint authority from a field that was never reviewed as one.
The Surface may offer a one-click "promote to contributed folder" that goes
through the ordinary grant route.

## 6. Credential slot resolution

At admission, the daemon walks every `CredentialSlot` in the profile and asks
each resolver, in order, whether it can satisfy the slot **on this node**:

| resolver | source | Phase 2 support |
| --- | --- | --- |
| `oauth:<provider>` | `ocean-oauth` auth store, provider block present and unexpired | yes |
| `env:<NAME>` | daemon process environment | yes |
| `keychain:<service>/<account>` | OS keychain | recorded, refused as `resolver_not_open` |

The result is a per-slot **status**, never a value:
`resolved | missing | expired | resolver_not_open`. A `required` slot that is
not `resolved` refuses admission with a typed `credential_slot_missing` and
raises an operator attention item naming the slot, not the resolver output.

The resolved value, when one exists, reaches the turn only through the existing
provider-auth and tool-environment seams that already handle secrets today.
Phase 2 adds no new place a secret lives and no new path a secret travels.
`inspect` serves the status table and nothing else.

For a federated room, the existing `secrets/set` lane on the Bedrock workspace
proxy remains the way an owner pushes a slot into the room container. Phase 2
does not touch that lane; the profile simply lets the operator see that a slot
the profile requires has or has not been set there, once Phase 3 adds the
projection.

## 7. API semantics

All routes live under `/v1/rooms/persistent/{key}`.

| route | auth | stage |
| --- | --- | --- |
| `GET  .../inspect` | none | **2a (landed with this document)** |
| `GET  .../profile` | none | **2b (landed)** |
| `PUT  .../profile` | operator | **2b (landed)** |
| `GET  .../resources` | none | **2c (landed)** |
| `POST .../resources` | operator | **2c (landed)** |
| `GET  .../resources/{resource_id}` | none | **2c (landed)** |
| `POST .../resources/{resource_id}/suspend` | operator | **2c (landed)** |
| `POST .../resources/{resource_id}/resume` | operator | **2c (landed)** |
| `DELETE .../resources/{resource_id}` | operator | **2c (landed)** |
| `POST .../resources/{resource_id}/list` | binding + grant | 2d |
| `POST .../resources/{resource_id}/read` | binding + grant | 2d |

### 7.1 `GET .../inspect` (Stage 2a)

One read under one store lock. Serves:

```text
ok            true
room          { id, name, created_at, updated_at, closed, workspace_root, trigger_policy }
access        RoomAccessProjection (state, members, outbox)
federated     bool — a room credential exists; the bearer is never serialized
owner         { member_id, eligible } | null
execution     { node: "local", cwd: <string|null>, cwd_source: "room_workspace_root" | "unbound" }
agents[]      binding projection (Phase 1 §8) plus:
                session_id        the deterministic (room, agent, generation) session
                session_exists    whether that session file is on disk
                execution_node    "local"
profile       null until Stage 2b; then RoomProfile minus nothing (it holds no secrets)
credential_slots[]   [] until Stage 2b; then { name, required, status } per §6
resources[]   [] until Stage 2c; then RoomResourceProjection per architecture §7.4
```

Unknown room is 404. A soft-closed room is served with `closed: true`, the same
rule `snapshot` follows. The route is deliberately credential-free: every field
is already served by an existing credential-free route or is derived from one,
and the two fields that are new (`session_id`, `session_exists`) name a session
the agent already owns.

### 7.2 Error codes

`room_not_found`, `invalid_room_key`, `phase_not_open`, `resolver_not_open`,
`credential_slot_missing`, `dangerous_root`, `root_already_granted`,
`resource_not_found`, `resource_not_available`, `stale_generation`,
`agent_not_authorized_for_resource`, `path_escapes_root`. Each is a typed body
`{ ok: false, code, error }` and writes nothing.

## 8. Audit

Every grant, suspend, resume, revoke, and profile write appends one
content-minimal System line to the room transcript in the Phase 1 §10 style:
actor, action, `resource_id` or `profile revision`, generation. Never the root,
never a slot value, never a resolver result.

Every `list` and `read` appends one row to a new `room_resource_audit` table
(room, resource, agent, generation, operation class, relative-path digest,
byte count, outcome). Relative paths are digested, not stored, per Decision 13.

## 9. Tests

Stage 2a (with this document):

- `inspect` on an unknown room is 404 with `room_not_found`.
- `inspect` on a room with no workspace reports `cwd: null`,
  `cwd_source: "unbound"`, `profile: null`, `resources: []`.
- `inspect` on a room with a bound workspace reports it and `cwd_source:
  "room_workspace_root"`.
- `inspect` on a room with an active binding reports the same `session_id`
  the convene path derives, and `session_exists: false` before any turn.
- `inspect` never serializes a bearer: a federated fixture's body contains no
  substring of the installed credential.

Stage 2b (landed with the profile routes):

- store: first write is revision 1 with one content-minimal audit row (counts
  and revision, no remote, no slot name); a second decision bumps to 2;
  identical replay is idempotent with no audit; changed content under a
  consumed id is refused and writes nothing; an id consumed by the
  agent-binding ledger cannot write a profile; unknown room is an error.
- routes: `GET` is 404 on an unknown room and `profile: null` before any
  write; `PUT` without the operator header is 503 and a typed validation
  refusal from an authenticated operator leaves no row and no audit line;
  `PUT` creates (201), replays (200, `changed: false`), refuses changed
  content (409 `decision_replay_mismatch`), and a fresh decision bumps the
  revision; `GET`, `PUT`, and `inspect` serve byte-identical `profile` and
  `credential_slots`.
- resolution: `env:` reports presence only; `oauth:` reads block presence and
  `expires` from the daemon's own `auth.json` and never the token; `expired`
  outranks `resolver_not_open` outranks `missing`; no projection ever
  contains a value.
- admission: a `required` slot that does not resolve refuses the room-agent
  turn with an audit row naming the slot, and the agent never speaks.

Stage 2c (landed with the resource routes):

- store: a grant is generation 1 and `available` with an audit row that names
  the id and alias but never the root; one live grant per canonical root
  until revoked, and a revoked root may be granted again as a new id; an
  expired live grant reads `revoked` at every check and is retired in the
  same transaction when its root is granted again; suspend/resume/revoke each
  bump the generation, a no-op transition consumes the decision without
  bumping, and revoked is terminal; replay is idempotent and a decision
  consumed by any other ledger is refused; unknown room and unknown grant
  are errors.
- daemon: every §4 dangerous root is refused by name (`/`, `$HOME` itself, a
  symlinked root, a non-directory, plus not-absolute, dot components, and
  missing); confinement is on the canonical RESULT — an in-folder symlink
  pointing outside is `path_escapes_root` while one pointing inside resolves;
  the safe projection never carries `local_root` or the digest, and an
  expired grant projects as `revoked`; an agent's `execution` projection
  carries `resource_id` and `grant_generation` but never a grant's path.
- routes: unauthenticated grant is 503 and writes nothing; dangerous and
  malformed roots each answer their own code and write nothing; a real grant
  is 201, exact replay 200 `created:false`, same root under a new decision
  409 `root_already_granted`; list, get, and `inspect` serve one identical
  rootless projection; suspend → resume → revoke bump generations 2 → 3 → 4
  and a transition out of revoked is 409; the audit ledger is exactly
  `granted, suspended, resumed, revoked, granted` with no root in any row.
- the cwd rule: a profile referencing a grant that does not exist is 400
  `resource_not_found` before any digest; with no workspace and no grant the
  agent is `unbound`; after a grant naming the agent and an `agent_defaults`
  entry, `inspect` reports `resource_grant` with the id and generation and
  no `cwd`, and a mention actually convenes the agent and it replies;
  suspending the grant makes the same agent `unbound` again.

Stage 2d adds its own list before opening; the pattern is Phase 1 §12.

## 10. Rollout gates

- Stage 2a may land on green CI with ordinary review; it is read-only.
- Stage 2b opens after operator acceptance of this document.
- Stage 2c opens after 2b lands and the dangerous-root list in §4 has a test
  per entry.
- Stage 2d opens after 2c lands and a replay test proves a stale generation is
  refused.
- Phase 3 does not open until every stage here is landed and the §9 suites are
  green.

## 11. Open questions — ruled 2026-09-08

Each was answered by the operator on 2026-09-08 and is binding.

1. **`keychain:` resolution waits for the remote-worker isolation ruling.**
   A keychain read is a broader capability than an env read. In 2b the scheme
   is accepted by validation and every slot naming it reports
   `resolver_not_open`; `oauth:` and `env:` are the only live resolvers.
2. **A `RepoRef` may have no `resource_id`.** A room that is "about" a repo
   before anyone contributes a checkout is a real state. `resource_id` remains
   optional, and when present must name a live grant on this node (2c).
3. **`inspect` does not serve audit rows.** The transcript's System-line stream
   is the audit view; a dedicated audit projection is Phase 3 work if the
   Surface asks for it.
4. **The default folder is per-agent, with a room default as fallback.** Two
   agents in one room working in different folders is the normal case. §2.1
   gains `agent_defaults` — a map from `agent_member_id` to `resource_id` — and
   §5 rule 1 reads the agent's entry first, then `default_resource_id`. Both
   are validated the same way and both stay `null`/empty until 2c can name a
   grant.
