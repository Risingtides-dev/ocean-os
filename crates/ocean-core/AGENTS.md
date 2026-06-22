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

## Work Guidance

- Prefer explicit fields and stable enums over implicit client-specific conventions.
- Update downstream crates when shared types change.
- Document any migration or compatibility risk in the root `events.md` entry for the work.

## Verification

- `cargo test -p ocean-core`
- `cargo check --workspace`

## Child devlog Index

No child boundaries defined within `ocean-core/` at this time.
