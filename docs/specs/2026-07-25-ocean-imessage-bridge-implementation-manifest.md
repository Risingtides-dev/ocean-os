# Ocean iMessage Bridge — Local Adapter Implementation Manifest

**Status:** implementation slice 1 landed; operator-managed only; autonomous extension integration requires a separately ratified privileged-service contract
**Date:** 2026-07-25
**Parent:** [Ocean Extensions Architecture and Migration Manifest](2026-07-14-ocean-extensions-architecture-and-migration-manifest.md)

## Decision

`integrations/ocean-imessage/` owns a local macOS adapter for exactly one
operator-approved iMessage pair:

```text
inbound:  +15717451650 -> +17035081859
outbound: +15717451650 only
```

It is a client adapter and future extension-service package, not an Ocean core
hook, daemon plugin, Ocean Buddy adapter, or a general Messages bridge.

## Landed slice

The native Swift package contains:

- a fail-closed, exact-E.164 admission filter before daemon delivery;
- a read-only SQLite reader for the private Messages database that requires the
  exact local destination identity, one remote participant, `iMessage`, an
  inbound text row, and no attachment/reaction fields;
- opaque-only local cursor/idempotency state with one reply per accepted message
  and a 20-per-hour hard cap;
- a direct Apple-event sender whose API has no recipient field, whose literal
  recipient is the approved remote number, and which requires an
  operator-enrolled explicit Messages account ID rather than choosing an account
  by ordering;
- a documented daemon HTTP/SSE client that creates a fresh session per allowed
  message and consumes only that session's response; and
- tests for pair admission, non-persistence of body text, replay prevention,
  reply rate limiting, and identifier parsing.

The repository change does not install, launch, or grant permissions. A local
operator may separately install a stable signed helper and explicitly grant
FDA/Automation, but that process remains outside extension activation.

## Invariants

1. Rejected Messages records never reach Ocean, stdout, bridge state, or logs.
2. The reader is the only component that may receive Full Disk Access. Ocean,
   generic shells/interpreters, and the sender do not receive it.
3. The sender must not expose a recipient parameter, use Accessibility, fall
   back to GUI scripting, choose an account implicitly, or send unless the
   opaque incoming ID was admitted.
4. Unknown private-schema fields, missing local identity proof, read failure,
   sender ambiguity, invalid config mode, oversized input/output, or rate-limit
   exhaustion fail closed.
5. Message body is untrusted input. It cannot configure the pair or daemon and
   cannot grant the adapter any filesystem/Messages privilege.
6. No raw credential belongs in the extension manifest or state.
7. Admitted content may be delivered only to `http://127.0.0.1:4780`; config permissions do not authorize another scheme, host, port, path, query, fragment, or credential.
8. Opaque admission and reply claims use a cross-process lock; a duplicate watcher must not submit or send the same message twice.

## Known boundary and next gate

macOS has no supported account-wide Messages watch API. The reader depends on
private `chat.db` schema plus Full Disk Access, while sending requires broad
Messages Automation consent. This is local direct-distribution software; it is
not App Store compatible.

`ocean-extension.toml` intentionally declares package metadata only, with no
executable service resource. Ocean's current Stage-A service protocol is stdio
NDJSON and rejects filesystem/network capabilities; it cannot safely express a
reader with FDA plus a separate Messages Automation sender. Before autonomous
extension integration, a separately ratified privileged-service contract and
authenticated package-attributed inbound seam must provide bounded process
lifetime, minimal environment, scoped state, explicit FDA/Automation ownership,
startup/restart/shutdown handling, audit identity, and no widening of
permissions. Do not use `ocean-hooks` as a substitute.

## Validation

```bash
swift test --package-path integrations/ocean-imessage
swift build -c release --package-path integrations/ocean-imessage
cargo xtask docs-check
```
