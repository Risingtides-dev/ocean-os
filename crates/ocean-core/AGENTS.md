# ocean-core — Shared Protocol Types

## Purpose

This crate owns shared protocol types used across Ocean clients, daemon, runtime, and SDK surfaces: requests, responses, events, sessions, and common data structures.

## Ownership

- **Scope:** `crates/ocean-core/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Primary responsibilities:** stable shared types, serialization contracts, cross-crate API compatibility

## Local Contracts

- Treat public type changes as cross-crate contract changes.
- Preserve serde compatibility unless the breaking change is intentional and documented.
- Keep protocol types free of daemon/runtime implementation details.
- Session synchronization uses a bounded `SessionSyncSnapshot` plus opaque
  boot-local `SessionEventFence`; it carries the persisted monotonic
  legacy-default-zero model `config_revision`, excludes raw messages, tool
  rows/payloads, thinking, and image metadata, caps visible user/assistant text
  at 512 rows and 1 MiB, and reports truncation counts. `AgentReplayGap` is reset-required and
  never claims ordering/range semantics for UUID event ids.
- `PermissionMode` wire names are stable (`manual`, `automatic`, `skip_all`);
  clients display daemon-reported saved/effective settings rather than deriving
  policy locally.
- The closed Track-0 `RoomId` projection family is retired. Durable room contracts use the open `RoomKey` model; do not recreate projection DTOs without a new audited API design.

### Federation types

- `RoomMessage.federated: Option<FederatedMessageMeta>` is the sole additive
  field on a G1 type — `#[serde(default)]`, backward-compatible.
- `FederatedMessageMeta`, `FederatedRoomMemberProjection`, `FederatedActorType`,
  `FederatedRoomRole`, `MemberPresence` (`live` | `unavailable`; no `stale`),
  `PublicAgentDescriptor`, `RoomOutboxItem`, `OutboxItemState`,
  `RoomAccessProjection`, `RoomAccessState`, `InviteResponse`,
  `RoomRedeemResponse` are owned here. `RoomRedeemResponse` is the redeem 200
  and `#[serde(flatten)]`s the projection beside a required `room_key`, so that
  reply's top level stays the projection's own keys plus the one new key; the
  key is not a projection field because the projection is also the per-room SSE
  frame. `InviteResponse.onboard_url: Option<String>` is additive and
  `skip_serializing_if`, so a `None` re-serializes to the original four keys
  rather than to a null an older surface has to learn to ignore; only the daemon
  composes it, because only it knows its own Bedrock origin. The URL embeds the
  invite code, which makes it a bearer grant and not a pointer to one — it
  belongs in the mint reply and nowhere else, and no fixture here may spell it
  with a live-shaped code.
- Invite request bodies are not mirrored here. `POST .../invites` and
  `POST .../invites/redeem` are deserialized by the daemon's own
  `CreateInviteBody`/`RedeemInviteBody` under `deny_unknown_fields`, which is
  the only shape a request is ever checked against; a `Serialize`-only twin in
  this crate is a second contract nothing validates and it will drift.
- Every new struct field is required unless individually `#[serde(default)]` or
  `skip_serializing_if`. No `Room`, `RoomParticipant`, `RoomMessageKind`, or
  `RoomTriggerPolicy` changes. Do not remove `Serialize`/`Deserialize` from
  types the daemon routes use.
- `PublicAgentDescriptor` explicitly transforms live `GET /v1/agents` fields;
  local paths, provider credentials, tool config, and permission posture are
  NEVER included. Forbidden keys (`owner_principal_token_id`,
  `provider_api_key`, `execution_role`, `local_paths`, `tool_config`,
  `permission_posture`) must not survive serde roundtrip on any type.
- `ocean-store` and `ocean-daemon` own their implementation surfaces; this
  crate owns the type definitions + exhaustive serde round-trip/enum/backward
  compat + required-field + forbidden-key tests.

## Work Guidance

- Prefer explicit fields and stable enums over implicit client-specific conventions.
- Update downstream crates when shared types change.
- Document any migration or compatibility risk in the root `events.md` entry for the work.

## Verification

- `cargo test -p ocean-core`
- `cargo check --workspace`

## Child devlog Index

No child boundaries defined within `ocean-core/` at this time.
