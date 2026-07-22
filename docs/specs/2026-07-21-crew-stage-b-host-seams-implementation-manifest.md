# Crew Stage B — Generic Host Seams Implementation Manifest

**Status:** proposed (awaiting operator acceptance)
**Date:** 2026-07-21
**Scope:** implementation ratification for **Stage B only** — the generic, orchestration-agnostic host seams §6.1–6.3, 6.5–6.6 of the parent manifest. No Crew/orchestration code. No UI artifact lane (§6.4, a later stage).
**Parent contract:** [`2026-07-18-ocean-crew-orchestration-and-durable-workflow-manifest.md`](2026-07-18-ocean-crew-orchestration-and-durable-workflow-manifest.md) (operator-accepted 2026-07-21, Stage A authorized; §8 requires each of Stages B–E to have its own implementation manifest before code — this is Stage B's).
**Pattern:** mirrors [`2026-07-17-observatory-gate1-implementation-manifest.md`](2026-07-17-observatory-gate1-implementation-manifest.md) — a normative, test-gated implementation contract, not a code change by itself.

---

## 1. Decision

Ocean-os core gains **five generic host seams** that let any activated extension (deploys, imports, approval flows, and — later — Crew) request ordinary turns, cancel them, observe their lifecycle, request continuations, and keep confined per-package state. The seams interpret **no orchestration vocabulary**: core never learns what a crew, worker, subagent, graph, join, undertow, or offshore delegate is. Every seam is gated by activation grants and ordinary permission policy, and **no seam can widen a grant**.

This manifest makes the parent's §6 seams concrete and testable. It is the gate the parent's §8 requires before any Stage B code lands.

## 2. Why a manifest, not a wave

- The parent (§8) forbids Stage B code without a ratified Stage-B implementation manifest, mirroring Observatory Gate 1.
- The capability-intersection rule (§6.1) is a **security boundary**: a wrong intersection silently widens a delegated turn's authority beyond the operator's grant. It must be specified and tested before code, exactly like the observatory auth contract.
- Three agents (claude, thoth, pi) independently converged on scoped, per-boot, daemon-held capability tokens as the auth model. This manifest is where that consensus becomes a normative host contract.

## 3. Scope

**In** (the five Stage B seams, parent §6.1–6.3, 6.5–6.6):
1. Extension execution request (§6.1)
2. Generic cancellation (§6.2)
3. Scoped lifecycle delivery (§6.3)
4. Continuation request (§6.5)
5. Extension state directory (§6.6)

**Out** (explicit, fail-the-review if present):
- The UI artifact lane / `workflow_control` render kind (§6.4) — a separate stage.
- Any core type, field, enum, or string named `Crew`, `Worker`, `Subagent`, `GraphNode`, `Join`, `task`, `spawn_worker`, `delegate_undertow`, `delegate_offshore`, `undertow`, `offshore`, or a fleet/named-worker runtime (parent §5, §9.1).
- The Longhouse delegation facade tools (Stage D).
- Any credential handed to an extension process (see §4.6).

## 4. Normative seam contracts

### 4.1 Extension execution request (§6.1)

**Request carries:** package/actor audit identity; workspace/session binding; requested generic provider-route identity (a capability-provider id string, **not** a `local`/`offshore` enum); requested model; requested capability set; opaque correlation id; idempotency key; optional Observatory execution binding.

**Returns:** a durable **host execution id** (minted and owned by the host; the extension never chooses it).

**Effective capability computation (the load-bearing rule):**

```
effective = member_request
          ∩ parent_delegable          // what the delegating session is itself allowed to sub-delegate
          ∩ extension_activation_grant // what THIS package was activated with
          ∩ operator_policy            // standing permission policy
          ∩ workspace_session_ceilings // cwd/session bounds, budget, model allowlist
```

- **Fail closed:** a capability that is absent, unknown, or unparseable is treated as *not granted*, never as a wildcard. The empty set is a valid (fully-denied) result.
- **No widening by target:** the provider-route/target choice is applied *after* the intersection and can only narrow, never re-add, a capability. There is no code path where selecting a route grants a capability the intersection removed.
- **Audit:** both the **requested** and **effective** capability sets are recorded against the host execution id and are audit-visible. A reviewer can always see what was asked vs granted.

### 4.2 Auth: scoped, per-boot, daemon-held capability token (3-agent consensus)

- The extension process **holds no provider credentials**. The daemon holds all secrets and performs every provider call.
- An activated extension authenticates to the host with a **scoped capability token minted at daemon boot** from the persisted activation grant (the Observatory auth pattern, `c2d700a2` — reuse `ObserverToken`/`ObserverSecret` mint+verify primitives, do not reinvent). The token binds to the daemon boot instance; it never survives a restart and is re-minted on boot.
- A subagent/member turn is therefore **least-privilege by construction**: it can never exceed the operator's authority, escape permission gating, or spend past a ceiling, because it never possesses a key — it asks the daemon to act within `effective`.

### 4.3 Generic cancellation (§6.2)

- Cancel by host execution id. The host remains the sole process/session cleanup authority.
- Cancellation is idempotent (cancel-after-finish and double-cancel are no-ops, not errors).

### 4.4 Scoped lifecycle delivery (§6.3)

- A service receives lifecycle facts **only** for executions it owns or was explicitly granted: turn start/finish, tool start/finish metadata, permission waiting/resolved, cancellation, model reroute, session interruption.
- **No transcript, no tool-argument payloads** beyond the already-ratified Phase-2 observer metadata envelope. Delivery extends that vocabulary; it does not widen the envelope. (Widening the metadata envelope is a stop-and-consult trigger, parent §11.)
- Scoping is host-injected, never extension-asserted: a service cannot subscribe to another package's or another session's executions.

### 4.5 Continuation request (§6.5)

- Request **one** ordinary, package-attributed turn in an originating session, carrying bounded structured results.
- Deduplicated (by command id), rate-limited, audit-visible, and **rejected if the session no longer permits it** (revoked grant, ended session, changed policy).
- Explicitly **not** an interceptor and **not** context injection: it appends a normal turn at a turn boundary; it cannot mutate or inject into a running member turn (parent §7.9, §10).

### 4.6 Extension state directory (§6.6)

- A confined per-package state directory under the daemon-owned local state root (extension manifest §12.1).
- The extension may read/write only its own directory. It **never** touches daemon session JSON, another package's directory, or anything outside the confinement root. Path traversal / symlink escape out of the confinement root fails closed (reuse the TASK-30 no-follow discipline for reads that cross into it).

## 5. Durability & at-least-once recovery

- Execution requests carry **idempotency keys**; continuation/UI commands carry **command ids**. Retries are explicit **new attempts with new host execution ids** — the host never silently reuses an id.
- Completion reconciliation queries **host truth before relaunching anything**: after a daemon or service restart, the host is the authority on what actually ran; the extension reconciles against it and recovers unknown/in-flight work as **paused**, never as silently-succeeded or silently-relaunched.
- No exactly-once assumption anywhere. If SQLite recovery cannot reconcile with host truth without one, that is a stop-and-consult trigger (parent §11).

## 6. Where the code lands

- Generic seam handlers in `ocean-daemon` (a new `extension_seams.rs` or equivalent), exposed to activated extensions through the existing extension host, **carrying no Crew types**.
- Capability-token mint/verify **reuses** `ocean-observatory`'s `ObserverToken`/`ObserverSecret` primitives (do not fork them).
- The confinement root reuses the extension manifest §12.1 state root; no new global path.
- `ocean-longhouse` stays advisory/read-only — it gains nothing here (parent §5).

## 7. Acceptance gate (the conformance test matrix)

Stage B code is accepted only when host-conformance tests prove **all** of:

1. **Grant non-widening.** For a matrix of (member_request × parent_delegable × activation_grant × operator_policy × ceilings), `effective` equals the true set intersection, absent/unknown capabilities fail closed, and **no target/route choice re-adds a removed capability**. Exhaustive over the capability enum, not sampled.
2. **Session isolation.** A service cannot request, cancel, observe, or continue an execution it does not own or was not granted; cross-package and cross-session attempts are refused.
3. **Idempotent replay.** Re-submitting an execution request with the same idempotency key does not double-launch; replayed continuation command ids do not double-append; double/late cancellation is a no-op.
4. **Dedup & rate-limit.** The continuation seam dedups by command id and enforces its rate limit under burst.
5. **Audit identity.** Requested and effective capability sets, package/actor identity, and host execution id are recorded and retrievable for every execution.
6. **Package-removal safety.** Removing/deactivating a package cleanly invalidates its token, refuses its in-flight seam calls, and leaves no orphaned execution the operator can't see.
7. **No orchestration vocabulary in core.** A source-characterization test (like the daemon's existing boundary guards) asserts none of the forbidden identifiers from §3 appear in the core seam code.
8. **Auth token lifecycle.** Boot-minted tokens verify within the boot instance, fail closed across a simulated restart (re-mint required), and a forged/expired/wrong-instance token is refused (reuse the observatory auth test shape).

## 8. Stop-and-consult triggers (parent §11)

Halt and consult the operator if: a seam cannot be expressed without orchestration vocabulary in core; scoped delivery cannot be authenticated without weakening the metadata envelope; continuation semantics conflict with session authority; or SQLite recovery cannot reconcile with host truth without an exactly-once assumption.

## 9. Non-goals restated

This manifest authorizes **only** the five generic seams and their auth/durability/audit contract. It does **not** authorize the UI artifact lane, the Longhouse facade tools, lane adapters, capability *profiles* content (Stage D), the budget ladder, or any Crew runtime. Those remain behind their own stages and manifests.
