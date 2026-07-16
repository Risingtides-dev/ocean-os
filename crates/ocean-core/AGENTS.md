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

## Work Guidance

- Prefer explicit fields and stable enums over implicit client-specific conventions.
- Update downstream crates when shared types change.
- Document any migration or compatibility risk in the root `events.md` entry for the work.

## Verification

- `cargo test -p ocean-core`
- `cargo check --workspace`

## Gate-2 S2-P1 — Federation types (2026-07-16)

This crate owns the v2.2 wire types added for the producer-contract freeze
(`gate2-s2-p1-producer-freeze.md` v1.1 + `gate2-s2-wire-freeze.md` v2.2):

- `FederatedMessageMeta` — confirmed-federation metadata on `RoomMessage`
- `FederatedRoomMemberProjection` + `FederatedActorType` + `FederatedRoomRole`
- `MemberPresence` — derived presence (`live`, `unavailable`; no `stale`)
- `PublicAgentDescriptor` — safe agent projection (no credentials/paths)
- `RoomOutboxItem` + `OutboxItemState` — pending/failed outgoing events
- `RoomAccessProjection` + `RoomAccessState` — surface-facing snapshot
- `CreateInviteRequest` (Serialize only), `InviteResponse`, `RedeemInviteRequest`

`RoomMessage.federated: Option<FederatedMessageMeta>` is the sole additive
field on a G1 type — `#[serde(default)]`, backward-compatible. Every new
struct field is required unless individually `#[serde(default)]` or
`skip_serializing_if`. No `Room`, `RoomParticipant`, `RoomMessageKind`, or
`RoomTriggerPolicy` changes. Do not remove `Serialize`/`Deserialize` from
types the daemon routes use.

`ocean-store` and `ocean-daemon` own their implementation surfaces; this
crate owns the type definitions + exhaustive serde round-trip/enum/backward
compat tests (see `#[cfg(test)] mod tests`).

## Child devlog Index

No child boundaries defined within `ocean-core/` at this time.
