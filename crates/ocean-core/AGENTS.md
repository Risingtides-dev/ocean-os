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
  `RoomAccessProjection`, `RoomAccessState`, `CreateInviteRequest` (`Serialize`
  only), `InviteResponse`, `RedeemInviteRequest` are owned here.
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
