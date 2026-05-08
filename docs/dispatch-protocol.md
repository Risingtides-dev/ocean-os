# Twin dispatch protocol

> Version 1.0 — defines the contract between the Ocean-OS orchestrator and any twin's bridge.

## Overview

The orchestrator sends tasks to twins over HTTP. Each twin's bridge exposes one endpoint. Every request is signed with HMAC-SHA256 using a per-twin shared secret. The twin executes the task and returns a structured result envelope. The orchestrator handles timeouts and escalations.

This document is the single source of truth for implementors. Any twin operator should be able to build a conforming bridge without follow-up questions.

---

## 1. Task envelope

The orchestrator POSTs a JSON body to the twin's bridge. This is the **task envelope**.

### Shape

```jsonc
{
  "v": "1",                         // protocol version — always "1" for now
  "task_id": "<uuid-v4>",           // globally unique; used for idempotency and reply routing
  "issued_at": "<ISO 8601 UTC>",    // e.g. "2026-05-08T14:30:00.000Z" — used for replay check
  "timeout_ms": 30000,              // orchestrator will hard-cancel if reply is not received in time
  "origin": {
    "twin": "orchestrator",         // name of the sending entity
    "session_id": "<uuid-v4>"       // Claude session that originated the task
  },
  "task": {
    "type": "<task-type>",          // namespaced string, e.g. "ocean.query_campaign"
    "args": { }                     // type-specific payload (see §1.1)
  },
  "context": {                      // optional — ambient grounding data the twin may use
    "channel_id": "<string>",
    "thread_ts": "<string>",
    "user_id": "<string>"
  }
}
```

### Field reference

| Field | Type | Required | Notes |
|---|---|---|---|
| `v` | `"1"` | yes | Hard-coded for this version. Future breaking changes bump this. |
| `task_id` | UUID v4 string | yes | Twins must be idempotent on `task_id`. Re-delivering the same ID is a no-op; return the cached result. |
| `issued_at` | ISO 8601 UTC string | yes | Used for replay protection (see §3.3). |
| `timeout_ms` | integer ≥ 1000 | yes | Milliseconds. Orchestrator will not wait longer than this. Twins should self-cancel and return a timeout result if they cannot complete in time. |
| `origin.twin` | string | yes | Identity of the sender — always `"orchestrator"` for orchestrator-initiated tasks. |
| `origin.session_id` | UUID v4 string | yes | Claude session ID for tracing. |
| `task.type` | string | yes | Namespaced verb, format `<namespace>.<action>`. |
| `task.args` | object | yes | May be `{}` for zero-argument tasks. Never `null`. |
| `context` | object | no | Ambient Slack context. Omit if not applicable. |

### 1.1 Task types

Task types are namespaced strings. The initial set:

| Type | Args |
|---|---|
| `ocean.query_campaign` | `{ "slug": "<string>" }` |
| `ocean.search_threads` | `{ "query": "<string>", "limit": <int, default 10> }` |
| `ocean.deployments_for_repo` | `{ "name": "<string>", "since": "<ISO 8601>" }` |
| `ocean.creator_history` | `{ "handle": "<string>" }` |
| `ocean.post_content_to_telegram` | `{ "folder": "<string>", "brief": "<string>" }` |
| `ocean.diagnose_deploy` | `{ "repo": "<string>", "commit": "<string>" }` |
| `ocean.log_agent_action` | `{ "action": "<string>", "args": {}, "result": {}, "outcome": "<string>" }` |

New types are registered by opening a PR to this file. Twins must return `status: "unsupported_task"` (see §4.2) for types they do not recognise.

---

## 2. Inbound endpoint

Every twin's bridge **must** expose:

```
POST /twin/dispatch
Content-Type: application/json
```

### Requirements

- HTTPS only. Plain HTTP requests must be rejected with `403 Forbidden`.
- The endpoint must be reachable at the URL registered for that twin in the orchestrator's config.
- Must respond with the result envelope (§4) or an error result (§4.2) within `timeout_ms`.
- If the twin cannot respond in time, it must return a `timeout` result rather than leaving the connection open.
- Health check: `GET /twin/health` → `200 OK` with `{ "status": "ok" }`. Used by the orchestrator for pre-dispatch liveness checks.

### Response codes

| HTTP status | Meaning |
|---|---|
| `200 OK` | Task was received and processed. Body is a result envelope (even for logical errors — see §4). |
| `400 Bad Request` | Malformed JSON body or missing required fields. Body: `{ "error": "<message>" }`. |
| `401 Unauthorized` | Signature missing or invalid. |
| `403 Forbidden` | Non-HTTPS or IP not in allowlist (if the twin enforces one). |
| `409 Conflict` | `task_id` already processed and no cached result available (rare; prefer returning the cached result). |
| `429 Too Many Requests` | Rate limit. Orchestrator will back off and retry. |
| `5xx` | Unexpected error. Orchestrator treats this the same as a timeout result. |

**Important:** HTTP 4xx/5xx from the bridge are transport-level errors. The orchestrator distinguishes them from logical errors (which arrive inside a `200 OK` result envelope with a non-`"ok"` status). Do not use HTTP error codes to express business logic.

---

## 3. Authentication

### 3.1 Shared secret

Each twin is provisioned one secret: a 32-byte random value, hex-encoded, stored in the orchestrator's config and the twin's environment variables. The env var name convention is `OCEAN_DISPATCH_SECRET`.

Secrets are per-twin. If one twin's secret is compromised, only that twin's channel is affected.

### 3.2 Signing requests

The orchestrator signs every request with HMAC-SHA256.

**What is signed:** the raw UTF-8 bytes of the request body (the JSON task envelope, no normalisation).

**Algorithm:**

```
signature = HMAC-SHA256(key=secret, message=body_bytes)
header_value = "sha256=" + hex(signature)
```

The signature is sent in the HTTP header:

```
X-OceanOS-Signature: sha256=<64-char lowercase hex>
```

**Example (Python):**

```python
import hmac, hashlib

def sign(secret_hex: str, body: bytes) -> str:
    secret = bytes.fromhex(secret_hex)
    sig = hmac.new(secret, body, hashlib.sha256).hexdigest()
    return f"sha256={sig}"
```

**Example (TypeScript):**

```typescript
import { createHmac } from "crypto";

function sign(secretHex: string, body: string): string {
  const secret = Buffer.from(secretHex, "hex");
  const sig = createHmac("sha256", secret).update(body, "utf8").digest("hex");
  return `sha256=${sig}`;
}
```

### 3.3 Verifying requests (twin side)

Bridges **must** perform all three checks on every request:

#### Check 1 — header present

If `X-OceanOS-Signature` is absent, return `401`.

#### Check 2 — signature correct

Recompute the HMAC over the raw request body using the twin's stored secret. Compare using a **constant-time** comparison function (e.g. `hmac.compare_digest` in Python, `timingSafeEqual` in Node). If the comparison fails, return `401`.

Never use `==` for signature comparison — it is vulnerable to timing attacks.

#### Check 3 — replay protection

Parse `issued_at` from the request body. If the timestamp is more than **5 minutes** in the past or in the future, return `401` with body `{ "error": "timestamp out of window" }`.

This window accounts for clock skew while blocking replayed requests. Twin bridges must have NTP-synchronised clocks.

**Pseudocode:**

```python
def verify(request, secret_hex):
    sig_header = request.headers.get("X-OceanOS-Signature")
    if not sig_header:
        return 401

    expected = sign(secret_hex, request.body)
    if not hmac.compare_digest(sig_header, expected):
        return 401

    payload = json.loads(request.body)
    issued_at = datetime.fromisoformat(payload["issued_at"].rstrip("Z")).replace(tzinfo=timezone.utc)
    delta = abs((datetime.now(timezone.utc) - issued_at).total_seconds())
    if delta > 300:
        return 401

    return 200
```

---

## 4. Result envelope

Twins return a result envelope in the `200 OK` response body for all outcomes — success, logical error, timeout, or escalation. Never return a bare value; always wrap in this shape.

### Shape

```jsonc
{
  "v": "1",
  "task_id": "<uuid-v4>",        // must echo the task_id from the request
  "status": "<status-code>",      // see §4.1
  "completed_at": "<ISO 8601 UTC>",
  "result": { },                  // present and non-null when status is "ok"
  "error": {                      // present when status is not "ok"
    "code": "<error-code>",       // machine-readable (see §4.2)
    "message": "<string>",        // human-readable, for logs
    "retryable": <boolean>        // true if the orchestrator should retry
  },
  "meta": {                       // optional diagnostic info
    "duration_ms": <integer>,
    "twin": "<twin-name>"
  }
}
```

### 4.1 Status codes

| Status | Meaning |
|---|---|
| `"ok"` | Task completed successfully. `result` is populated. |
| `"error"` | Task failed with a recoverable or non-recoverable error. See `error` field. |
| `"timeout"` | Twin hit `timeout_ms` before completing. Orchestrator may retry. |
| `"escalate"` | Twin cannot handle this task autonomously; human review required. |
| `"unsupported_task"` | `task.type` is not implemented by this twin. |

### 4.2 Error codes

| Code | Meaning | Retryable |
|---|---|---|
| `"not_found"` | Requested resource does not exist. | false |
| `"upstream_error"` | A dependency (Postgres, Slack API, etc.) returned an error. | true |
| `"rate_limited"` | Upstream rate limit hit. | true |
| `"timeout"` | Self-reported timeout. | true |
| `"unsupported_task"` | Unknown task type. | false |
| `"internal"` | Unexpected error in the twin. | true |

### 4.3 Escalation

When a twin returns `status: "escalate"`, the `error` block carries:

```jsonc
{
  "code": "escalate",
  "message": "<why this task needs human review>",
  "retryable": false
}
```

The orchestrator routes escalations to a human operator via Slack DM on the configured escalation channel. Twins should escalate when:

- The task requires a judgment call outside the twin's domain.
- A downstream action (e.g. posting content) would have irreversible effects and the twin lacks sufficient confidence.
- An upstream API repeatedly fails in ways that suggest a real incident rather than a transient error.

---

## 5. Timeout semantics

- The orchestrator sets a deadline equal to `issued_at + timeout_ms`.
- If the twin has not replied by the deadline, the orchestrator drops the connection and records a `timeout` outcome.
- Twins **should** self-cancel and return a `timeout` result slightly before the deadline (e.g. `timeout_ms - 500ms`) so the orchestrator receives a structured response rather than a connection drop.
- Timed-out tasks are retried up to **3 times** with exponential backoff (2 s, 4 s, 8 s) before the orchestrator gives up and escalates to a human.

---

## 6. Retry and idempotency

- Twins must be idempotent on `task_id`. If the same `task_id` arrives twice, the twin returns the original result (from cache or storage) without re-executing the task.
- Orchestrator retries are semantically identical re-deliveries of the same task envelope (same `task_id`, same `issued_at`). The signature is re-computed over the same body bytes — bridges must accept the original `issued_at` within the 5-minute replay window.
- Side-effecting tasks (e.g. `ocean.post_content_to_telegram`) must guard against double-execution using the `task_id` as an idempotency key in whatever downstream system they call.

---

## 7. Worked example

### Scenario

The orchestrator wants to query a campaign's current state and sends the task to a twin that owns the `ocean.query_campaign` tool.

### Step 1 — orchestrator builds the task envelope

```json
{
  "v": "1",
  "task_id": "f47ac10b-58cc-4372-a567-0e02b2c3d479",
  "issued_at": "2026-05-08T14:30:00.000Z",
  "timeout_ms": 30000,
  "origin": {
    "twin": "orchestrator",
    "session_id": "8b3a2e1d-4f5c-4a7b-9d0e-1f2a3b4c5d6e"
  },
  "task": {
    "type": "ocean.query_campaign",
    "args": {
      "slug": "lazy-rosana-q2-2026"
    }
  },
  "context": {
    "channel_id": "C04ABCD1234",
    "thread_ts": "1715179200.000100",
    "user_id": "U03XYZ9876"
  }
}
```

### Step 2 — orchestrator signs and sends

```
POST https://jake-bridge.risingtides.ai/twin/dispatch
Content-Type: application/json
X-OceanOS-Signature: sha256=3d9e2f1a8c7b4e5d6f0a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e
```

Body is the JSON above.

### Step 3 — twin verifies and executes

The twin's bridge:
1. Extracts `X-OceanOS-Signature` from headers.
2. Recomputes HMAC-SHA256 over the raw request body using its stored `OCEAN_DISPATCH_SECRET`.
3. Compares with `timingSafeEqual`. Match — proceed.
4. Parses `issued_at`: `2026-05-08T14:30:00.000Z`. Current time is `2026-05-08T14:31:12.000Z`. Delta = 72 s < 300 s — pass.
5. Checks `task_id` against its idempotency store — not seen before — proceed.
6. Executes `ocean.query_campaign({ slug: "lazy-rosana-q2-2026" })` against Ocean's Postgres.

### Step 4 — twin returns the result envelope

```json
{
  "v": "1",
  "task_id": "f47ac10b-58cc-4372-a567-0e02b2c3d479",
  "status": "ok",
  "completed_at": "2026-05-08T14:31:12.847Z",
  "result": {
    "slug": "lazy-rosana-q2-2026",
    "name": "Lazy Rosana Q2 2026",
    "status": "active",
    "budget_usd": 12000,
    "spent_usd": 4850,
    "creators": [
      { "handle": "@lazymusicfan", "posts_owed": 3, "posts_delivered": 2 },
      { "handle": "@rosanavibez", "posts_owed": 2, "posts_delivered": 2 }
    ],
    "performance_summary": {
      "total_views": 182400,
      "avg_engagement_rate": 0.048
    }
  },
  "meta": {
    "duration_ms": 847,
    "twin": "jake-bridge"
  }
}
```

HTTP status: `200 OK`.

### Alternate: twin cannot complete in time

If the Postgres query takes longer than `timeout_ms - 500ms`, the twin self-cancels and returns:

```json
{
  "v": "1",
  "task_id": "f47ac10b-58cc-4372-a567-0e02b2c3d479",
  "status": "timeout",
  "completed_at": "2026-05-08T14:30:29.490Z",
  "error": {
    "code": "timeout",
    "message": "Query did not complete within the 30 s window.",
    "retryable": true
  },
  "meta": {
    "duration_ms": 29490,
    "twin": "jake-bridge"
  }
}
```

HTTP status: `200 OK`. The orchestrator records the timeout and schedules a retry with exponential backoff.

---

## 8. JSON Schema

Full JSON Schema definitions for both envelopes.

### Task envelope schema

```json
{
  "$schema": "https://json-schema.org/draft/2020-12",
  "title": "TaskEnvelope",
  "type": "object",
  "required": ["v", "task_id", "issued_at", "timeout_ms", "origin", "task"],
  "additionalProperties": false,
  "properties": {
    "v":          { "type": "string", "const": "1" },
    "task_id":    { "type": "string", "format": "uuid" },
    "issued_at":  { "type": "string", "format": "date-time" },
    "timeout_ms": { "type": "integer", "minimum": 1000 },
    "origin": {
      "type": "object",
      "required": ["twin", "session_id"],
      "additionalProperties": false,
      "properties": {
        "twin":       { "type": "string" },
        "session_id": { "type": "string", "format": "uuid" }
      }
    },
    "task": {
      "type": "object",
      "required": ["type", "args"],
      "additionalProperties": false,
      "properties": {
        "type": { "type": "string", "pattern": "^[a-z_]+\\.[a-z_]+$" },
        "args": { "type": "object" }
      }
    },
    "context": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "channel_id": { "type": "string" },
        "thread_ts":  { "type": "string" },
        "user_id":    { "type": "string" }
      }
    }
  }
}
```

### Result envelope schema

```json
{
  "$schema": "https://json-schema.org/draft/2020-12",
  "title": "ResultEnvelope",
  "type": "object",
  "required": ["v", "task_id", "status", "completed_at"],
  "additionalProperties": false,
  "properties": {
    "v":            { "type": "string", "const": "1" },
    "task_id":      { "type": "string", "format": "uuid" },
    "status":       { "type": "string", "enum": ["ok", "error", "timeout", "escalate", "unsupported_task"] },
    "completed_at": { "type": "string", "format": "date-time" },
    "result":       { "type": "object" },
    "error": {
      "type": "object",
      "required": ["code", "message", "retryable"],
      "additionalProperties": false,
      "properties": {
        "code":      { "type": "string" },
        "message":   { "type": "string" },
        "retryable": { "type": "boolean" }
      }
    },
    "meta": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "duration_ms": { "type": "integer" },
        "twin":        { "type": "string" }
      }
    }
  }
}
```

---

## 9. Implementation checklist

For any twin operator building a conforming bridge:

- [ ] `POST /twin/dispatch` endpoint on HTTPS
- [ ] `GET /twin/health` returns `{ "status": "ok" }`
- [ ] Validates `X-OceanOS-Signature` header using constant-time comparison
- [ ] Rejects requests with `issued_at` older than 5 minutes or in the future
- [ ] Stores/checks `task_id` for idempotency (in-memory cache with TTL ≥ 10 min is fine)
- [ ] Returns all responses as result envelopes (even errors) with HTTP `200`
- [ ] Returns `unsupported_task` for unknown `task.type` values
- [ ] Self-cancels and returns `timeout` result before `timeout_ms` elapses
- [ ] Logs `task_id`, `task.type`, `status`, and `duration_ms` for every dispatch
