# Ocean iMessage — fixed-pair local adapter

This macOS-only adapter admits exactly one conversation before Ocean receives a
single byte of message content:

- **inbound:** `+15717451650` → local identity `+17035081859`
- **outbound:** only `+15717451650`

It rejects outgoing rows, SMS, any group conversation, attachments, reactions,
missing/ambiguous local identity, non-E.164 identifiers, overlong text, and
unknown Messages schemas. Rejected rows are never printed, written to state, or
sent to Ocean. The state file holds only an opaque row/message ID, a cursor,
and reply idempotency/rate-limit metadata — never message text.

> The Messages database is a private macOS implementation detail, not a public
> API. This is signed/direct-distribution-only software, not an App Store
> integration. A schema or permission anomaly fails closed.

## Security boundary

`chat.db` requires Full Disk Access. Grant FDA **only to a signed reader helper**
when this is packaged for operation; never to `ocean-daemon`, a shell, Python,
Node, or a general agent runtime. The sender separately needs Automation access
to Messages and accepts only `(accepted-message-id, text)` — it has no recipient
argument and uses a literal compiled `+15717451650`. It selects only the explicit,
operator-enrolled Messages account ID; it never falls back to the first account,
uses Accessibility, or uses GUI scripting.

The repository package is a reviewable native adapter, not an autonomously
managed extension service. A local operator may install a stable signed helper
identity and explicitly grant FDA/Automation after reviewing the fixed-pair
policy, but that installation remains operator-managed. The extension manifest
intentionally declares no executable resource because Ocean's current stdio,
capability-free service contract cannot safely express this privileged boundary.

## Build and tests

```bash
swift test --package-path integrations/ocean-imessage
swift build -c release --package-path integrations/ocean-imessage
```

`poll` is the safe inspection mode: it outputs only admitted JSON lines. It does
not contact Ocean or Messages for sending.

```bash
integrations/ocean-imessage/.build/release/ocean-imessage poll
```

`run` is intentionally not an installation instruction. It uses the documented
local daemon HTTP/SSE APIs and requires a mode-0600 config file:

```json
{
  "daemonURL": "http://127.0.0.1:4780/",
  "cwd": "/absolute/registered/workspace",
  "messagesAccountID": "operator-verified-iMessage-account-id"
}
```

The operator must enroll the exact authenticated account ID after manually
verifying it is the `+17035081859` identity in Messages; Apple provides no
public account-ID-to-phone-number proof. Without it, sending fails closed. The
adapter creates a fresh Ocean session for each accepted message, waits for only
that session's SSE reply, limits outbound text to 1,000 characters, and allows
at most 20 replies/hour. The supplied iMessage text remains untrusted
agent input: it cannot select a recipient, alter the fixed pair, change bridge
configuration, or gain Messages database access.

## Extension status

`ocean-extension.toml` contains package metadata only and declares no service.
Autonomous extension integration requires a separately ratified privileged
service protocol with authenticated package attribution, bounded lifetime,
minimal environment, scoped state, FDA/Automation ownership, and explicit
startup/restart/shutdown behavior. The current Stage-A stdio service contract
and capability policy do not provide that boundary; do not substitute
`ocean-hooks`, Ocean Buddy, or a raw daemon route.
