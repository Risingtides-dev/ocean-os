# ocean-imessage — Local macOS iMessage Adapter

## Purpose

This package is the local-only, allowlisted macOS adapter for one Ocean iMessage conversation. It is a client adapter, not a daemon plugin or alternate agent runtime.

## Ownership

- **Scope:** `integrations/ocean-imessage/`
- **Parent:** `../AGENTS.md`
- **Runtime authority:** Ocean daemon, `ocean-agent`, and `ocean-runtime` retain session, model, tool, permission, and persistence authority.

## Local Contracts

- The fixed pair is inbound `+15717451650` → local `+17035081859`; outbound delivery is only to `+15717451650`.
- Filter in the native reader before any content reaches Ocean. Never emit, log, persist, or forward rejected Messages rows.
- The reader is the sole component that may receive Full Disk Access; the daemon, shell wrappers, and agent process must not receive it.
- The sender accepts a prior accepted message ID and text, never a destination. It must reject unknown/replied IDs and never use Accessibility/UI scripting.
- Read Messages `chat.db` read-only. Its schema is private and unsupported; an unknown/ambiguous schema or identity is a fail-closed condition.
- Do not claim App Store compatibility. This is a signed, local direct-distribution adapter and requires explicit FDA/Automation consent.
- The current executable is a bounded, operator-managed local adapter. Its `run` mode may deliver content only to `http://127.0.0.1:4780` through documented daemon HTTP/SSE APIs. The extension manifest intentionally declares no executable resource: current supervised-service contracts cannot express this FDA/Automation boundary, so autonomous extension integration requires a separately ratified privileged-service protocol. Do not reuse `ocean-hooks` or Ocean Buddy.

## Work Guidance

- Keep message payloads out of diagnostics and state. State may retain only opaque IDs/cursors and send status.
- Preserve the one-to-one iMessage-only, no-attachment/no-reaction, exact-E.164 checks and reply byte/rate limits.
- Any widening of the fixed pair, direct DB write, generic recipient parameter, GUI scripting fallback, or source of credentials is a stop-and-consult change.

## Verification

```bash
swift test --package-path integrations/ocean-imessage
swift build --package-path integrations/ocean-imessage
```

## Child devlog Index

- `Sources/` — native reader, daemon client, ledger, and fixed-recipient sender.
- `Tests/` — allowlist, ledger, and protocol safety tests.
