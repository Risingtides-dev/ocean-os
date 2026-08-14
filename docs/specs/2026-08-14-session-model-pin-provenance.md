# Session model-pin provenance

Status: operator-approved implementation contract, 2026-08-14.

## Problem

Ocean persisted a session's `model` and `provider` but not whether that model
was explicitly pinned through `PATCH /v1/agent/sessions/{id}/config`. The daemon
inferred pin authority by comparing the session model with the current global
model. An explicit pin equal to the global model was therefore reported as
`model_source: global`, so callers could not prove model identity before a turn.

## Decision

Use the session's persisted monotonic `config_revision` as model-pin
provenance:

| Stored value | Meaning |
| --- | --- |
| `0` | New inherited session or legacy session without mutation provenance. |
| `> 0` | At least one explicit session-config mutation occurred. |

New inherited sessions begin at revision zero. The session-config mutation
stores the catalog-resolved model/provider and advances the revision under the
existing session operation lease. One
`SessionModelConfig::is_session_pinned` resolver drives both turn selection and
the config RPC's `model_source`, so projection cannot disagree with execution.

## Compatibility

The revision field is additive upstream state and old session JSON deserializes
at zero without migration. Revision-zero records retain the former behavior: a
non-empty model different from the current global model is treated as pinned. A
legacy explicit pin equal to the global model has no recoverable provenance; an
operator must re-PATCH that session once after upgrade, or use a fresh session.
No transcript, cwd, provider, or public turn payload is rewritten.

## Acceptance

- A new session reports `model_source: global`.
- Explicitly pinning that session to the same model as global reports
  `model_source: session` after a persistence round trip.
- A new unpinned session follows later global-model changes.
- An explicit pin remains authoritative independently of global-model equality.
- Revision-zero inherited and legacy records preserve the prior equality fallback.
- Malformed/unknown config requests, locking, event emission, and sanitized
  404/409/500 behavior remain unchanged.

Source anchors are `crates/ocean-agent/src/session/mod.rs`,
`crates/ocean-agent/src/lib.rs`, and `crates/ocean-daemon/src/main.rs`. Focused
verification is `cargo test -p ocean-agent session_model_pin`,
`cargo test -p ocean-agent same_model_session_pin_persists_explicit_provenance`,
and `cargo test -p ocean-daemon session_config_ -- --nocapture`.

## Rollout

Build and restart the daemon from the reviewed main revision, create fresh GLM
and DeepSeek sessions, verify config identity/cwd before dispatch, and complete
the Stitchpad READY → artifact → commit → CLOSED proof. Existing legacy
equal-global sessions are evidence only and are not reused for acceptance.
