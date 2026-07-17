# Ocean Observatory — Gate 1 Implementation Manifest

**Date:** 2026-07-17
**Status:** Implementation specification pending operator acceptance
**Owner:** ocean-observatory crate (core daemon authority)
**Scope:** All contracts and type definitions that downstream tasks (2–9) depend on
**Prerequisite:** Gate 0 decisions accepted in `2026-07-17-observatory-gate0-decisions.md`

---

## Overview

This manifest specifies the complete Rust type system, persistence contract, authentication model, API routes, and test fixtures required to implement Ocean Observatory v1 (Gate 1 tasks 2–9).

**Every code task depends on sections 1–7 of this manifest.** Deviations require an amendment to this document and explicit approval before implementation.

The manifest enforces:
- Forbidden-field redaction via compile-time type constraints (not runtime filters)
- Metadata-only content policy
- Read-only observer semantics
- Extension ownership of subagent semantics
- Operator-safe credential distribution

---

## 1. Rust Type Definitions

All types are defined in the `ocean-observatory` crate at `crates/ocean-observatory/src/types.rs` and re-exported from `crates/ocean-observatory/src/lib.rs`.

### 1.1 Cursor: Monotonic Daemon-Owned Allocation

```rust
/// Persistent durable event ordering. Internal representation is monotonic u64 (never decreases).
/// Wire format is decimal string to prevent JavaScript BigInt issues.
/// Allocated by daemon only, never by clients or extensions.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Cursor(u64);

impl Cursor {
    /// Minimum valid cursor (first event).
    pub const MIN: Cursor = Cursor(1);
    
    /// Allocate the next cursor. Panics if source has been exhausted (u64::MAX).
    pub fn next(self) -> Cursor {
        Cursor(self.0.checked_add(1).expect("cursor overflow"))
    }
    
    /// Get the internal u64 representation.
    pub fn inner(self) -> u64 {
        self.0
    }
    
    /// Parse from decimal string wire format. Rejects non-decimal, leading zeros, or overflow.
    pub fn from_wire(s: &str) -> Result<Cursor, CursorParseError> {
        if s.is_empty() {
            return Err(CursorParseError::Empty);
        }
        if s.starts_with('0') && s.len() > 1 {
            return Err(CursorParseError::LeadingZero);
        }
        let n: u64 = s.parse().map_err(|_| CursorParseError::Invalid)?;
        Ok(Cursor(n))
    }
    
    /// Serialize to decimal string wire format.
    pub fn to_wire(&self) -> String {
        self.0.to_string()
    }
}

impl Serialize for Cursor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_wire())
    }
}

impl<'de> Deserialize<'de> for Cursor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Cursor, D::Error> {
        let s = String::deserialize(deserializer)?;
        Cursor::from_wire(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorParseError {
    Empty,
    LeadingZero,
    Invalid,
}
```

**Gap Semantics:**
- A missing cursor range between `after` and `before` signals one of: daemon restart, retention boundary exceeded, or temporary unavailability.
- Clients receive explicit `gap` responses (section 7.3).
- Cursors are monotonic: never skip forward, never decrement.
- Events arriving out of order (e.g., from a delayed background task) are rejected at admission time, not coalesced.

---

### 1.2 Producer Identity

```rust
/// Authority that created/attested an event.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Producer {
    /// Daemon-owned observation (request/runtime/permission adapters).
    Daemon {
        /// Fixed string: "ocean-daemon"
        id: String,
    },
    /// Extension-attested topology metadata.
    Extension {
        /// Extension identity (stable across restarts).
        id: String,
    },
}

impl Producer {
    pub fn is_daemon(&self) -> bool {
        matches!(self, Producer::Daemon { .. })
    }
    
    pub fn is_extension(&self) -> bool {
        matches!(self, Producer::Extension { .. })
    }
    
    pub fn id(&self) -> &str {
        match self {
            Producer::Daemon { id } | Producer::Extension { id } => id,
        }
    }
}
```

---

### 1.3 Truth Provenance

```rust
/// Authority and method of fact recording.
#[derive(Clone, Debug, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TruthProvenance {
    /// Recorded by daemon-owned adapters (runtime, request, permission, tool).
    /// Authority: daemon host observation.
    HostObserved,
    
    /// Topology metadata reported by an activated extension.
    /// Authority: extension claim, validated by daemon.
    ExtensionAttested,
    
    /// Derived projection (aggregated, filtered, or folded from other events).
    /// Authority: none (computed state).
    Derived,
}
```

---

### 1.4 Visibility / Scope

```rust
/// Observer principal scope. Enforced at API route authorization layer.
#[derive(Clone, Debug, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Topology, lifecycle phases, safe aliases, metrics, duration, byte counts.
    /// No prompts, output, thinking, errors, paths, environment, or decision secrets.
    Metadata,
    
    /// Reserved for future: bounded access to prompts/output with explicit approval.
    /// Not implemented in v1.
    Content,
}
```

---

### 1.5 Topology: Execution Identities and Relationships

```rust
/// Observable execution node identity and parent/root relationships.
/// Immutable once recorded. Retries generate new execution IDs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Topology {
    /// One root or child execution attempt.
    /// Unique, immutable, daemon-allocated.
    pub execution_id: Uuid,
    
    /// Immediate parent execution (null only for root executions).
    /// Immutable. Must be validated at admission time (no cycles, reasonable depth).
    pub parent_execution_id: Option<Uuid>,
    
    /// Root of the execution tree.
    /// For root executions: same as execution_id.
    /// For children: reference to the topmost ancestor.
    /// Immutable.
    pub root_execution_id: Uuid,
    
    /// Parent-child edge identity (minted by daemon, only when parent is non-null).
    /// Used for lookup and mutation coordination.
    /// Null for root executions.
    pub edge_id: Option<Uuid>,
    
    /// Existing host-owned transcript correlation (session authority).
    pub session_id: Uuid,
    
    /// Existing host-owned transcript correlation (turn authority).
    pub turn_id: Uuid,
    
    /// Existing host-owned transcript correlation (request authority).
    pub request_id: Uuid,
}

impl Topology {
    /// Verify invariants:
    /// - root_execution_id is never null
    /// - if parent_execution_id is null, root_execution_id == execution_id
    /// - if parent_execution_id is some, edge_id is some
    pub fn validate(&self) -> Result<(), TopologyError> {
        if self.parent_execution_id.is_none() && self.root_execution_id != self.execution_id {
            return Err(TopologyError::RootMismatch);
        }
        if self.parent_execution_id.is_some() && self.edge_id.is_none() {
            return Err(TopologyError::EdgeMissing);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum TopologyError {
    RootMismatch,
    EdgeMissing,
}
```

---

### 1.6 Correlation: Activity References (Not Topology)

```rust
/// Activity correlations. These are NOT topology nodes; they are references
/// to ongoing or completed work in other transcript authorities.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Correlation {
    /// Tool call identity (if this event is related to a tool invocation).
    /// Not a graph node; only for correlation with tool transcript.
    pub tool_call_id: Option<Uuid>,
    
    /// Permission query identity (if this event awaits or resolves a permission).
    /// Not a graph node; only for correlation with permission authority.
    pub permission_id: Option<Uuid>,
}
```

---

### 1.7 EventKind: Typed Event Enum

```rust
/// Typed event payload kinds. All variants are metadata-only (no content, secrets, or raw text).
/// Each variant maps to a specific struct payload (section 1.8).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum EventKind {
    // Daemon lifecycle
    DaemonStarted(DaemonStartedPayload),
    DaemonStopped(DaemonStoppedPayload),
    
    // Execution lifecycle
    ExecutionAdmitted(ExecutionAdmittedPayload),
    ExecutionBinding(ExecutionBindingPayload),
    ExecutionPhaseChanged(ExecutionPhaseChangedPayload),
    ExecutionHeartbeat(ExecutionHeartbeatPayload),
    ExecutionFinished(ExecutionFinishedPayload),
    
    // Tool activity
    ToolStarted(ToolStartedPayload),
    ToolFinished(ToolFinishedPayload),
    
    // Permission activity
    PermissionWaiting(PermissionWaitingPayload),
    PermissionResolved(PermissionResolvedPayload),
    
    // Model routing
    ModelRerouted(ModelReroutedPayload),
    
    // Topology validation
    TopologyAdmissionRejected(TopologyAdmissionRejectedPayload),
}
```

---

### 1.8 Event Payloads (Metadata-Only)

```rust
// ===== Daemon Lifecycle =====

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonStartedPayload {
    /// ISO 8601 timestamp when daemon process started.
    pub started_at: DateTime<Utc>,
    /// Reason for start: "boot", "reload", "migration", etc.
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonStoppedPayload {
    /// ISO 8601 timestamp when daemon stop was initiated.
    pub stopped_at: DateTime<Utc>,
    /// Stop reason: "shutdown", "crash", "timeout", etc.
    pub reason: String,
}

// ===== Execution Lifecycle =====

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionAdmittedPayload {
    /// Safe human-readable label (no prompts, no paths, no secrets).
    pub label: String,
    /// Fixed phase after admission: "pending".
    pub phase: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionBindingPayload {
    /// Indicator that binding was successful and token was validated.
    pub bound: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionPhaseChangedPayload {
    /// Previous phase: "pending", "admitted", "running", "finished", "error", "interrupted".
    pub from_phase: String,
    /// New phase.
    pub to_phase: String,
    /// ISO 8601 timestamp of the phase change.
    pub changed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionHeartbeatPayload {
    /// Monotonic sequence number within this execution.
    pub sequence: u64,
    /// ISO 8601 timestamp of heartbeat.
    pub heartbeat_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionFinishedPayload {
    /// Terminal phase: "success", "failure", "cancelled", "interrupted".
    pub phase: String,
    /// ISO 8601 timestamp when work finished.
    pub finished_at: DateTime<Utc>,
    /// Total duration in milliseconds (from admitted to finished).
    pub duration_ms: u64,
    /// Outcome code: "ok", "error", "cancelled", "timeout", etc. (fixed enum).
    pub outcome: String,
}

// ===== Tool Activity =====

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolStartedPayload {
    /// Safe tool name (no arguments, no paths, no transformation of the name).
    pub tool_name: String,
    /// Classification: "builtin", "local", "remote", "network", etc. (fixed enum).
    pub classification: String,
    /// ISO 8601 timestamp when tool invocation started.
    pub started_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolFinishedPayload {
    /// Safe tool name.
    pub tool_name: String,
    /// Classification.
    pub classification: String,
    /// ISO 8601 timestamp when tool invocation completed.
    pub finished_at: DateTime<Utc>,
    /// Total duration in milliseconds.
    pub duration_ms: u64,
    /// Outcome: "success", "error", "timeout", "cancelled" (fixed enum).
    pub outcome: String,
    /// Byte count of tool input (not the content, just the size).
    pub input_bytes: u64,
    /// Byte count of tool output.
    pub output_bytes: u64,
}

// ===== Permission Activity =====

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionWaitingPayload {
    /// Fixed reason code: "approval_needed", "user_interaction", "cost_limit", etc.
    pub reason: String,
    /// ISO 8601 timestamp when wait began.
    pub started_waiting_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionResolvedPayload {
    /// Fixed outcome code: "approved", "denied", "timed_out", "cancelled".
    pub outcome: String,
    /// ISO 8601 timestamp when resolution occurred.
    pub resolved_at: DateTime<Utc>,
    /// Duration of wait in milliseconds.
    pub wait_duration_ms: u64,
}

// ===== Model Routing =====

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelReroutedPayload {
    /// Safe model alias (not the full path or endpoint).
    pub model_alias: String,
    /// Fixed reason code: "rate_limit", "fallback", "cost_optimization", etc.
    pub reason: String,
    /// ISO 8601 timestamp of reroute.
    pub rerouted_at: DateTime<Utc>,
}

// ===== Topology Validation =====

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TopologyAdmissionRejectedPayload {
    /// Reason for rejection: "cycle_detected", "depth_exceeded", "parent_unknown",
    /// "cross_authority", "duplicate_edge", "invalid_idempotency_key".
    pub reason: String,
    /// ISO 8601 timestamp of rejection.
    pub rejected_at: DateTime<Utc>,
}
```

---

### 1.9 EventEnvelope: Complete Durable Record

```rust
/// Immutable durable event. This is the unit of persistence and stream transport.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventEnvelope {
    // ===== Schema & Versioning =====
    /// Always "1" for v1. Unsupported major versions are rejected.
    pub schema_version: u32,
    
    // ===== Durable Ordering =====
    /// Monotonic daemon-allocated cursor (primary sort key for replay).
    pub cursor: Cursor,
    
    // ===== Identities =====
    /// Unique idempotency key (for replay deduplication). UUID.
    pub event_id: Uuid,
    
    /// Persistent local observation authority identity. Stable across daemon restarts within
    /// the same host (may differ across host migrations or backups).
    pub observatory_id: Uuid,
    
    /// Daemon boot instance. Changes on each restart. Used to detect gaps and restarts.
    pub daemon_instance_id: Uuid,
    
    // ===== Timestamps =====
    /// When the fact occurred (source timestamp).
    pub occurred_at: DateTime<Utc>,
    
    /// When the fact was recorded into durable storage (daemon timestamp).
    pub recorded_at: DateTime<Utc>,
    
    // ===== Typed Event =====
    /// Tagged, typed event payload (see section 1.7–1.8).
    pub kind: EventKind,
    
    // ===== Authority =====
    /// Who recorded this fact: daemon adapter or extension attestation.
    pub truth: TruthProvenance,
    
    /// Producer identity (daemon or extension ID).
    pub producer: Producer,
    
    // ===== Topology & Correlation =====
    /// Execution node and parent/root relationships.
    pub topology: Topology,
    
    /// Activity references (not graph nodes).
    pub correlation: Correlation,
    
    // ===== Access Control =====
    /// Visibility scope (always Metadata for v1).
    pub visibility: Visibility,
}

impl EventEnvelope {
    /// Validate all invariants before persistence:
    /// - schema_version is supported
    /// - cursor is positive
    /// - all UUIDs are valid
    /// - timestamps are reasonable (recorded_at >= occurred_at, both within daemon clock skew)
    /// - topology is valid (see Topology::validate)
    /// - visibility matches producer (extensions can only attest, not observe)
    pub fn validate(&self) -> Result<(), EventValidationError> {
        if self.schema_version != 1 {
            return Err(EventValidationError::UnsupportedSchema);
        }
        if self.cursor.inner() == 0 {
            return Err(EventValidationError::InvalidCursor);
        }
        if self.recorded_at < self.occurred_at {
            return Err(EventValidationError::TimeSkew);
        }
        // Clock skew tolerance: 30 seconds
        let skew = (self.recorded_at - self.occurred_at).to_std()
            .unwrap_or(std::time::Duration::from_secs(31));
        if skew > std::time::Duration::from_secs(30) {
            return Err(EventValidationError::ExcessiveTimeSkew);
        }
        self.topology.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum EventValidationError {
    UnsupportedSchema,
    InvalidCursor,
    TimeSkew,
    ExcessiveTimeSkew,
    TopologyError(TopologyError),
}
```

---

## 2. Cursor Semantics: Monotonic Allocation and Gap Handling

### 2.1 Allocation

- **Daemon-owned:** Only the daemon allocates cursors. Extensions and clients never generate or claim cursors.
- **Monotonic increment:** Each new event receives `previous_cursor + 1`.
- **No gaps under normal operation:** Sequential events have consecutive cursors.
- **Panic on overflow:** If cursor reaches u64::MAX and another event arrives, panic. This signals a design error (cursor allocation too aggressive). Do not wrap or reset.

### 2.2 Gaps

Gaps occur in three scenarios:

1. **Daemon restart:** All nonterminal executions transition to `interrupted` phase. A `DaemonStopped` event is recorded with the last cursor from the previous instance, followed by `DaemonStarted` with a new daemon_instance_id. The cursor sequence is continuous, but daemon_instance_id changes signal a restart to clients.

2. **Retention boundary exceeded:** The oldest metadata event has cursor N; a client requests events after cursor M (M < N). The store returns a 410 (Gone) response with the new earliest available cursor, forcing the client to start from a later point.

3. **Temporary unavailability:** Network, database, or process issues prevent retrieving intermediate events. The store returns a 503 (Service Unavailable) or explicit gap event.

### 2.3 Client Semantics

- **Snapshot + tail:** Clients MUST not assume live events start immediately after snapshot watermark. They MUST check for explicit gap messages.
- **No silent attachment:** SSE never silently skips cursors. A gap is always explicit (section 7.3).
- **Explicit reset:** If a client loses position or the stream resets, the client must request a new snapshot.

---

## 3. Authentication and Authorization

### 3.1 Token Format and Generation

Observer tokens are **HMAC-SHA256 signed** using a daemon-local secret, or **opaque random tokens** validated against daemon-held state. The manifest requires HMAC-SHA256 as the primary approach.

#### Secret Storage

```
~/.ocean/observatory-secret
```

- **Location:** User home directory, specific to the daemon instance.
- **Permissions:** Mode `0600` (read/write by owner only).
- **Format:** 32 bytes of random binary data (encoded as hex for disk storage).
- **Rotation:** Not supported in v1. Tokens signed with a rotated secret become invalid; operators must re-authenticate.
- **Initialization:** Generated by daemon on first startup if missing.

#### Token Structure

The token is a JWT-like structure with three parts: header, payload, signature (all base64url-encoded and separated by dots).

```rust
/// Observer token payload (the "payload" part of the token).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenPayload {
    /// Fixed prefix: "observatory".
    pub token_type: String,
    
    /// Scoped principal: "summary" or "content" (v1: summary only).
    pub scope: String,
    
    /// Daemon instance this token is bound to (prevents cross-daemon token reuse).
    pub daemon_instance_id: String,
    
    /// Token issued timestamp (seconds since epoch).
    pub iat: i64,
    
    /// Token expiry timestamp (seconds since epoch).
    pub exp: i64,
}

impl TokenPayload {
    /// Create a new token payload with a default lifetime (15-60 min, configurable).
    pub fn new(daemon_instance_id: String, lifetime_secs: u64) -> TokenPayload {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        TokenPayload {
            token_type: "observatory".to_string(),
            scope: "summary".to_string(),
            daemon_instance_id,
            iat: now,
            exp: now + lifetime_secs as i64,
        }
    }
    
    /// Check if token is expired (exp < now).
    pub fn is_expired(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        self.exp < now
    }
}

/// Complete signed observer token.
pub struct ObserverToken {
    pub payload: TokenPayload,
    pub signature: Vec<u8>,
}

impl ObserverToken {
    /// Serialize to wire format (header.payload.signature).
    pub fn to_wire(&self) -> String {
        let header = base64_url::encode(&r#"{"alg":"HS256","typ":"JWT"}"#.as_bytes());
        let payload = base64_url::encode(&serde_json::to_vec(&self.payload).unwrap());
        let signature = base64_url::encode(&self.signature);
        format!("{}.{}.{}", header, payload, signature)
    }
    
    /// Parse and validate token from wire format.
    pub fn from_wire(token_str: &str, secret: &[u8]) -> Result<ObserverToken, TokenValidationError> {
        let parts: Vec<&str> = token_str.split('.').collect();
        if parts.len() != 3 {
            return Err(TokenValidationError::MalformedToken);
        }
        
        let payload_json = base64_url::decode(parts[1])
            .map_err(|_| TokenValidationError::InvalidEncoding)?;
        let payload: TokenPayload = serde_json::from_slice(&payload_json)
            .map_err(|_| TokenValidationError::InvalidPayload)?;
        
        if payload.is_expired() {
            return Err(TokenValidationError::Expired);
        }
        
        // Verify signature
        let signature_expected = hmac_sha256(&format!("{}.{}", parts[0], parts[1]).as_bytes(), secret);
        let signature_provided = base64_url::decode(parts[2])
            .map_err(|_| TokenValidationError::InvalidEncoding)?;
        
        if signature_expected != signature_provided {
            return Err(TokenValidationError::InvalidSignature);
        }
        
        Ok(ObserverToken { payload, signature: signature_provided })
    }
}

#[derive(Debug)]
pub enum TokenValidationError {
    MalformedToken,
    InvalidEncoding,
    InvalidPayload,
    Expired,
    InvalidSignature,
}

fn hmac_sha256(data: &[u8], key: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC key is valid");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}
```

### 3.2 Token Distribution

#### Web/Tauri Clients

- **Distribution:** Issued by the daemon proxy when a client connects, placed in a secure HTTP-only cookie.
- **Cookie name:** `ocean_observer_token`
- **Cookie attributes:** `secure`, `httponly`, `samesite=strict`, `path=/v1/observatory`
- **Lifetime:** 15–60 minutes (configurable, default 30 min).
- **Refresh:** When cookie expires, client must re-authenticate with the daemon (e.g., via Basic auth to the proxy, then receive a new cookie).

#### CLI/Extension Clients

- **Distribution:** Read from `OCEAN_OBSERVER_TOKEN` environment variable on daemon startup.
- **Source:** Daemon reads from `~/.ocean/observatory-secret` to sign the token internally, then passes it to child processes via `OCEAN_OBSERVER_TOKEN`.
- **Lifetime:** 15–60 minutes from daemon start (or custom, if configured).
- **Refresh:** Child processes re-read `OCEAN_OBSERVER_TOKEN` from environment or make a refresh request to daemon.

### 3.3 Authorization Scopes

| Scope | Access | v1 Support |
|-------|--------|-----------|
| `observatory:summary` | Topology, phases, safe names, metrics, duration, byte counts. No content, errors, paths, environment, secrets. | Yes (default) |
| `observatory:content` | Bounded prompts, output, thinking (future). Requires explicit opt-in and separate privacy decision. | No (Gate 2+) |
| `producer:<extension-id>` | Admit/renew/read only that producer's topology. | No (Gate 2+) |

**v1 default:** All authenticated observers receive `observatory:summary` scope. There is no hierarchical scope inheritance; `summary` does not grant `content`.

### 3.4 Token Lifecycle and Security

**Never persisted:**
- Tokens are never written to disk, logs, event streams, or database.
- Tokens are never embedded in URLs or query parameters.
- Tokens are never included in Observatory event payloads.

**Revocation:**
- Short-lived tokens (15–60 min) are revoked by expiry.
- Operator can revoke all tokens by changing the daemon-local secret (invalidates all existing tokens; requires re-authentication).

**Cross-daemon validation:**
- Each daemon has its own secret.
- A token signed by daemon A is invalid for daemon B (different `daemon_instance_id` and different HMAC key).

---

## 4. SQLite/WAL Persistence Contract

### 4.1 Schema

The `ocean-observatory` crate owns a dedicated SQLite database at `~/.ocean/observatory.db`.

#### CREATE TABLE: observatory_events

```sql
CREATE TABLE IF NOT EXISTS observatory_events (
    -- Primary key
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    
    -- Durable ordering
    cursor INTEGER UNIQUE NOT NULL,
    
    -- Event identity and deduplication
    event_id BLOB NOT NULL UNIQUE,  -- 16-byte UUID
    
    -- Timestamps
    occurred_at INTEGER NOT NULL,   -- Unix milliseconds
    recorded_at INTEGER NOT NULL,   -- Unix milliseconds
    
    -- Event envelope JSON (compact, all fields required)
    envelope BLOB NOT NULL,
    
    -- Indexes
    CONSTRAINT cursor_positive CHECK (cursor > 0)
);

CREATE INDEX idx_observatory_events_cursor ON observatory_events(cursor);
CREATE INDEX idx_observatory_events_recorded_at ON observatory_events(recorded_at);
CREATE INDEX idx_observatory_events_event_id ON observatory_events(event_id);
```

**Storage notes:**
- `envelope` is stored as BLOB (raw bytes, UTF-8 JSON).
- `cursor` is INTEGER (u64 in Rust, stored as SQLite INTEGER with range limits).
- `occurred_at` and `recorded_at` are INTEGER (Unix milliseconds since epoch, stored as i64).
- `event_id` is BLOB (16-byte UUID binary representation).

#### CREATE TABLE: execution_nodes

Projection table for fast topology queries (updated transactionally with event append).

```sql
CREATE TABLE IF NOT EXISTS execution_nodes (
    -- Node identity
    execution_id BLOB PRIMARY KEY,  -- 16-byte UUID
    root_execution_id BLOB NOT NULL,
    parent_execution_id BLOB,
    session_id BLOB NOT NULL,
    
    -- State
    phase TEXT NOT NULL,            -- "pending", "admitted", "running", "finished", "error", "interrupted"
    producer_kind TEXT NOT NULL,    -- "daemon" or "extension"
    producer_id TEXT NOT NULL,
    truth TEXT NOT NULL,            -- "host_observed", "extension_attested", "derived"
    
    -- Lifecycle
    admitted_at INTEGER NOT NULL,   -- Unix milliseconds (cursor of ExecutionAdmitted event)
    finished_at INTEGER,            -- Unix milliseconds (cursor of ExecutionFinished event, nullable if not terminal)
    
    FOREIGN KEY (parent_execution_id) REFERENCES execution_nodes(execution_id),
    CONSTRAINT phase_valid CHECK (phase IN ('pending', 'admitted', 'running', 'finished', 'error', 'interrupted'))
);

CREATE INDEX idx_execution_nodes_root ON execution_nodes(root_execution_id);
CREATE INDEX idx_execution_nodes_parent ON execution_nodes(parent_execution_id);
CREATE INDEX idx_execution_nodes_session ON execution_nodes(session_id);
CREATE INDEX idx_execution_nodes_phase ON execution_nodes(phase);
```

#### CREATE TABLE: execution_edges

Parent-child relationships (one edge per admission).

```sql
CREATE TABLE IF NOT EXISTS execution_edges (
    -- Edge identity
    edge_id BLOB PRIMARY KEY,       -- 16-byte UUID
    
    -- Relationship
    parent_execution_id BLOB NOT NULL,
    child_execution_id BLOB NOT NULL,
    
    -- Lifecycle
    admitted_at INTEGER NOT NULL,   -- Unix milliseconds (cursor at admission)
    disconnected_at INTEGER,        -- Unix milliseconds (if lease expires or child fails)
    
    FOREIGN KEY (parent_execution_id) REFERENCES execution_nodes(execution_id),
    FOREIGN KEY (child_execution_id) REFERENCES execution_nodes(execution_id),
    UNIQUE (parent_execution_id, child_execution_id)
);

CREATE INDEX idx_execution_edges_parent ON execution_edges(parent_execution_id);
CREATE INDEX idx_execution_edges_child ON execution_edges(child_execution_id);
```

#### CREATE TABLE: watermarks

Per-namespace/per-producer watermark (for pagination and snapshot consistency).

```sql
CREATE TABLE IF NOT EXISTS watermarks (
    -- Namespace
    namespace TEXT PRIMARY KEY,  -- e.g., "snapshot", "daemon_lifecycle"
    
    -- Position
    cursor INTEGER NOT NULL,
    recorded_at INTEGER NOT NULL,  -- Unix milliseconds
    
    CONSTRAINT cursor_positive CHECK (cursor > 0)
);
```

### 4.2 Store API Signatures

All methods are defined in `crates/ocean-observatory/src/store.rs`.

```rust
pub struct ObservatoryStore {
    db: tokio_rusqlite::Connection,
    retention_policy: RetentionPolicy,
}

/// Configuration for event retention.
pub struct RetentionPolicy {
    /// Maximum event age in seconds (default: 7 days = 604,800 seconds).
    pub max_age_secs: u64,
    /// Maximum storage size in bytes (default: 1 GiB = 1_073_741_824 bytes).
    pub max_size_bytes: u64,
}

impl ObservatoryStore {
    /// Open or create the database at ~/.ocean/observatory.db.
    pub async fn open(retention_policy: RetentionPolicy) -> Result<ObservatoryStore, StoreError> {
        // Initialize schema if needed
        // Load current watermarks
        // Run retention cleanup if needed
    }
    
    /// Append a single validated event to the log.
    /// Returns the assigned cursor if successful.
    pub async fn append(&mut self, envelope: EventEnvelope) -> Result<Cursor, StoreError> {
        // 1. Validate envelope
        // 2. Allocate next cursor
        // 3. Insert into observatory_events
        // 4. Update projections (execution_nodes, execution_edges)
        // 5. Update watermarks
        // 6. Return cursor
    }
    
    /// Get the latest cursor in the log.
    pub async fn latest_cursor(&self) -> Result<Option<Cursor>, StoreError> {
        // SELECT MAX(cursor) FROM observatory_events
    }
    
    /// Get the earliest cursor in the log (considering retention).
    pub async fn earliest_cursor(&self) -> Result<Option<Cursor>, StoreError> {
        // SELECT MIN(cursor) FROM observatory_events
    }
    
    /// Fetch a snapshot of current execution topology at a given cursor.
    /// Returns nodes, edges, pending-attention summaries, and capabilities.
    pub async fn snapshot(&self, at_cursor: Cursor) -> Result<SnapshotProjection, StoreError> {
        // Transactional read of execution_nodes and execution_edges state at cursor
    }
    
    /// Stream events after a given cursor.
    /// Returns an async iterator of EventEnvelope.
    pub async fn tail(&self, after: Cursor) -> Result<impl Stream<Item = Result<EventEnvelope, StoreError>>, StoreError> {
        // SELECT * FROM observatory_events WHERE cursor > after ORDER BY cursor
    }
    
    /// Fetch paginated events in a range.
    pub async fn replay(
        &self,
        after: Cursor,
        through: Option<Cursor>,
        limit: usize,
    ) -> Result<ReplayPage, StoreError> {
        // SELECT * FROM observatory_events
        // WHERE cursor > after AND (through IS NULL OR cursor <= through)
        // ORDER BY cursor LIMIT limit
        // Return: events, next_after, has_more, complete
    }
    
    /// Get current state of a single execution node.
    pub async fn execution_node(&self, execution_id: Uuid) -> Result<Option<ExecutionNodeProjection>, StoreError> {
        // SELECT * FROM execution_nodes WHERE execution_id = ?
    }
    
    /// Get children of an execution.
    pub async fn execution_children(&self, parent_execution_id: Uuid) -> Result<Vec<ExecutionNodeProjection>, StoreError> {
        // SELECT * FROM execution_nodes WHERE parent_execution_id = ?
    }
    
    /// Compact and prune events according to retention policy.
    /// Called periodically (e.g., on daemon restart or every N hours).
    pub async fn compact(&mut self) -> Result<(), StoreError> {
        // 1. Delete events older than max_age_secs
        // 2. Delete events if total size > max_size_bytes (prune oldest first)
        // 3. VACUUM database
        // 4. Update watermarks
    }
    
    /// Get current daemon instance and earliest/latest cursors.
    pub async fn status(&self) -> Result<StoreStatus, StoreError> {
        // SELECT daemon_instance_id (from latest event), MIN(cursor), MAX(cursor), ...
    }
}

#[derive(Debug)]
pub enum StoreError {
    DatabaseError(String),
    ValidationError(String),
    Conflict(String),  // Idempotency key collision or foreign key violation
    NotFound,
    Overflow,  // Cursor exhaustion
    RetentionBoundary,  // Requested cursor older than retention
}
```

### 4.3 Concurrency Model

- **Single writer, multiple readers:**
  - `ObservatoryStore` uses an async-aware mutex for writes.
  - Multiple read operations can proceed in parallel.
  - WAL mode is enabled (SQLite default for concurrent access).

- **Transaction isolation:**
  - Append transactions are serialized (one at a time).
  - Snapshot and replay transactions are read-only and can interleave with appends.
  - WAL ensures consistent reads across append boundaries.

- **Crash recovery:**
  - On daemon restart, WAL is replayed.
  - Incomplete appends are rolled back.
  - Watermarks are re-computed from the event log.

---

## 5. Admission and Binding Contract

### 5.1 Admission Request

An extension requests admission for a child execution:

```rust
/// Request to admit a child execution into the topology.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdmissionRequest {
    /// Safe label for the child execution (no prompts, paths, secrets).
    pub label: String,
    
    /// Parent execution identity. If null, this is a root execution (rare; usually root is daemon).
    pub parent_execution_id: Option<Uuid>,
    
    /// Immediate root of the tree (for cycle/depth checks). Must be valid and reachable.
    pub root_execution_id: Uuid,
    
    /// Producer identity (extension ID).
    pub producer_id: String,
    
    /// Lease TTL in seconds. After this duration, the child becomes `disconnected` if not renewed.
    /// v1 limits: 1 second (minimum) to 86400 seconds (24 hours).
    pub lease_secs: u64,
    
    /// Idempotency key (UUID). Duplicate keys in the same transaction window (e.g., 5 min)
    /// are deduplicated and return the same AdmissionResult.
    pub idempotency_key: Uuid,
}

impl AdmissionRequest {
    pub fn validate(&self, max_depth: usize) -> Result<(), AdmissionError> {
        if self.label.is_empty() || self.label.len() > 256 {
            return Err(AdmissionError::InvalidLabel);
        }
        if self.lease_secs < 1 || self.lease_secs > 86400 {
            return Err(AdmissionError::InvalidLeaseTtl);
        }
        // Additional depth/cycle checks happen in the store
        Ok(())
    }
}
```

### 5.2 Admission Result

The daemon returns:

```rust
/// Result of admission request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdmissionResult {
    /// Newly minted execution ID (or deduped ID if idempotency key matched).
    pub execution_id: Uuid,
    
    /// Root of the tree (same as request).
    pub root_execution_id: Uuid,
    
    /// One-time binding token (opaque, 32 bytes, random).
    /// Valid for 30 seconds. Must be included in the next turn's binding event.
    /// Never persisted, logged, or streamed.
    pub binding_token: Vec<u8>,  // 32 bytes
    
    /// Binding token expiry (absolute timestamp, Unix seconds).
    pub binding_expires_at: i64,
    
    /// Timestamp when admission was recorded.
    pub admitted_at: DateTime<Utc>,
}
```

### 5.3 Admission Validation Rules

1. **Depth limit:** Maximum 50 levels of nesting (prevent runaway graphs).
2. **Cycle detection:** No execution may be its own ancestor (check root_execution_id reachability).
3. **Cross-authority rejection:** Parent must belong to the same daemon instance (check daemon_instance_id).
4. **Parent existence:** Parent execution must be known (already admitted or root).
5. **Idempotency:** If the same idempotency_key is seen within 5 minutes, return the same AdmissionResult (cached).
6. **Lease bounds:** 1 second minimum, 24 hours maximum.
7. **Duplicate edge:** Only one parent-child edge per (parent_id, child_id) pair. Retries are new children (new execution_id), not edge replays.

### 5.4 Binding Token Lifecycle

```rust
pub struct BindingToken {
    /// Random 32 bytes (never regenerated).
    pub token: Vec<u8>,
    /// Issued timestamp (Unix seconds).
    pub issued_at: i64,
    /// Expiry timestamp (30 seconds later).
    pub expires_at: i64,
}

impl BindingToken {
    pub fn is_valid(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        self.expires_at > now
    }
}
```

**Lifecycle:**
1. Generated in AdmissionResult (in-memory only).
2. Passed to extension via API response.
3. Extension includes token in the next turn's binding event payload.
4. Daemon validates and consumes the token (removes from in-memory map).
5. After consumption, token is not stored anywhere.
6. After 30 seconds, token expires and is discarded (no cleanup needed, just validation fails).

**Storage:** Binding tokens are **never persisted** to database or logs. They are held in a time-bounded in-memory cache (e.g., Rust `HashMap` with a background task cleaning up expired tokens every minute).

---

## 6. Redaction and Property Test Fixtures

### 6.1 Forbidden Fields (Compile-Time Enforcement)

The following fields and content must **never appear in any persisted EventEnvelope**:

| Category | Forbidden | Reason |
|----------|-----------|--------|
| **Prompts & text** | `prompt`, `system_message`, `thinking`, `assistant_message`, `response_text` | Sensitivity, customer data |
| **Tool I/O** | `tool_input`, `tool_output`, `tool_arguments`, `tool_result` | Often contains data, secrets, or API responses |
| **Errors & traces** | `error_message`, `error_stacktrace`, `error_code` (unless fixed enum) | Leaks implementation details, file paths |
| **Execution env** | `environment_variables`, `cwd`, `working_directory`, `absolute_path` | Contains secrets and system state |
| **Secrets & auth** | `api_key`, `token`, `password`, `auth_header`, `bearer`, `secret` | Never log credentials |
| **Decisions & reasons** | `permission_arg`, `permission_reasoning`, `approval_reason` | Decision logic is sensitive |
| **Extension payloads** | Raw extension metadata, unauthenticated attestation content | Validate and redact before acceptance |

**Compile-time enforcement strategy:**
- Define `EventKind` enum and `*Payload` structs with ONLY safe fields (string names, fixed enum codes, durations, byte counts).
- Use Rust's type system: no `String` for content (use fixed enum); no `Option<String>` for optional errors (omit the field).
- Use serde's `#[serde(skip)]` or `#[serde(skip_serializing_if)]` to prevent accidental field inclusion.
- Use a property test (section 6.2) to verify no payload struct can serialize a forbidden field.

### 6.2 Property Test Fixture: Exhaustive Redaction Verification

Create a test in `crates/ocean-observatory/tests/redaction_property_tests.rs`:

```rust
#[cfg(test)]
mod redaction_property_tests {
    use ocean_observatory::*;
    use serde_json::json;
    
    /// Property test: every EventKind serializes without forbidden fields.
    #[test]
    fn prop_no_forbidden_fields_in_any_event() {
        // 1. Create representative EventEnvelopes for every EventKind variant
        let test_cases = vec![
            envelope_daemon_started(),
            envelope_daemon_stopped(),
            envelope_execution_admitted(),
            envelope_execution_phase_changed(),
            envelope_tool_started(),
            envelope_tool_finished(),
            envelope_permission_waiting(),
            envelope_permission_resolved(),
            envelope_model_rerouted(),
            envelope_topology_admission_rejected(),
        ];
        
        for envelope in test_cases {
            let json = serde_json::to_value(&envelope).expect("envelope serializes");
            
            // 2. Scan entire JSON tree for forbidden strings
            assert_no_forbidden_fields(&json, &envelope.kind);
        }
    }
    
    fn assert_no_forbidden_fields(value: &serde_json::Value, kind: &EventKind) {
        let forbidden_patterns = vec![
            "prompt", "thinking", "output", "error", "environment",
            "api_key", "token", "password", "secret", "auth",
            "stacktrace", "exception", "cwd", "path",
        ];
        
        let json_str = value.to_string();
        for pattern in forbidden_patterns {
            assert!(!json_str.contains(&pattern),
                "Forbidden pattern '{}' found in event kind {:?}",
                pattern, kind);
        }
    }
    
    fn envelope_daemon_started() -> EventEnvelope {
        EventEnvelope {
            schema_version: 1,
            cursor: Cursor(1),
            event_id: uuid::Uuid::new_v4(),
            observatory_id: uuid::Uuid::new_v4(),
            daemon_instance_id: uuid::Uuid::new_v4(),
            occurred_at: Utc::now(),
            recorded_at: Utc::now(),
            kind: EventKind::DaemonStarted(DaemonStartedPayload {
                started_at: Utc::now(),
                reason: "boot".to_string(),
            }),
            truth: TruthProvenance::HostObserved,
            producer: Producer::Daemon { id: "ocean-daemon".to_string() },
            topology: Topology { /* ... */ },
            correlation: Correlation { /* ... */ },
            visibility: Visibility::Metadata,
        }
    }
    
    // ... Similar builders for every EventKind variant ...
}
```

### 6.3 EventKind Variant Coverage

Every EventKind variant must have a corresponding property test case. The following variants are in scope for v1:

| Variant | Payload Type | Forbidden Fields to Verify |
|---------|--------------|--------------------------|
| `DaemonStarted` | `DaemonStartedPayload` | N/A (reason is fixed string) |
| `DaemonStopped` | `DaemonStoppedPayload` | N/A (reason is fixed string) |
| `ExecutionAdmitted` | `ExecutionAdmittedPayload` | label must not contain paths/prompts |
| `ExecutionBinding` | `ExecutionBindingPayload` | binding_token must not be in payload |
| `ExecutionPhaseChanged` | `ExecutionPhaseChangedPayload` | phase must be fixed enum |
| `ExecutionHeartbeat` | `ExecutionHeartbeatPayload` | N/A (only seq and timestamp) |
| `ExecutionFinished` | `ExecutionFinishedPayload` | outcome must be fixed enum; no error_message |
| `ToolStarted` | `ToolStartedPayload` | tool_name is safe; classification is fixed enum |
| `ToolFinished` | `ToolFinishedPayload` | No tool output/arguments; outcome is fixed enum |
| `PermissionWaiting` | `PermissionWaitingPayload` | reason is fixed enum; no permission_arguments |
| `PermissionResolved` | `PermissionResolvedPayload` | outcome is fixed enum; no approval_reasoning |
| `ModelRerouted` | `ModelReroutedPayload` | model_alias is safe; reason is fixed enum |
| `TopologyAdmissionRejected` | `TopologyAdmissionRejectedPayload` | reason is fixed enum |

---

## 7. API Route Contracts

All routes require an authenticated observer token (section 3). All responses include `Cache-Control: no-store`.

### 7.1 GET /v1/observatory/snapshot

**Purpose:** Fetch a transactionally consistent snapshot of current topology state.

**Request:**
```
GET /v1/observatory/snapshot?detail=<detail>
Authorization: Bearer <token>
Accept: application/json
```

Query parameters:
- `detail` (optional): `full` or `summary` (default). `summary` omits historical inactive bulk.

**Response (200 OK):**
```json
{
  "schema_version": 1,
  "snapshot_cursor": "1234",
  "snapshot_at": "2026-07-17T18:02:31.123Z",
  "daemon_instance_id": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
  "earliest_cursor": "1",
  "latest_cursor": "1234",
  "nodes": [
    {
      "execution_id": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
      "root_execution_id": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
      "parent_execution_id": null,
      "phase": "running",
      "label": "Root agent turn",
      "producer": "daemon",
      "truth": "host_observed",
      "admitted_at": "2026-07-17T18:02:00.000Z",
      "finished_at": null,
      "duration_ms": 31123
    }
  ],
  "edges": [
    {
      "edge_id": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
      "parent_execution_id": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
      "child_execution_id": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
      "admitted_at": "2026-07-17T18:02:05.000Z",
      "disconnected_at": null
    }
  ],
  "pending_attention": [
    {
      "execution_id": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
      "phase": "error",
      "reason": "Tool invocation failed"
    }
  ],
  "capabilities": ["snapshot", "tail", "replay"]
}
```

**Error responses:**
- **400 Bad Request:** Malformed query parameters.
- **401 Unauthorized:** Missing or invalid token.
- **410 Gone:** `snapshot_cursor` has been pruned by retention policy.
- **503 Service Unavailable:** Database unavailable.

---

### 7.2 GET /v1/observatory/events

**Purpose:** Tail live events from a resume point (SSE stream).

**Request:**
```
GET /v1/observatory/events?after=<cursor>
Authorization: Bearer <token>
Accept: text/event-stream
Last-Event-ID: <event_id>
```

Query parameters:
- `after` (optional): Resume from this cursor. Defaults to latest watermark.

Headers:
- `Last-Event-ID` (optional): Resume from this event ID (SSE standard).

**Response (200 OK, text/event-stream):**

```
data: {"schema_version":1,"cursor":"1235",...}
id: event-id-1
event: envelope

data: {"schema_version":1,"cursor":"1236",...}
id: event-id-2
event: envelope

data: {"reason":"cursor_not_found","after":"1250","earliest":"1000"}
id: gap-1
event: gap

:heartbeat
```

**SSE Event Types:**

1. **`envelope`** — Complete EventEnvelope JSON.
   ```
   data: {"schema_version":1,"cursor":"...","kind":...}
   id: <event_id>
   event: envelope
   ```

2. **`gap`** — Indicates missing events (retention boundary, restart, or temporary unavailability).
   ```json
   {
     "reason": "cursor_not_found|retention_boundary|restart",
     "after": "1249",
     "earliest": "1000",
     "latest": "1300"
   }
   ```

3. **Heartbeat** — Sent every 30 seconds if no events (proves connection is alive).
   ```
   :heartbeat
   ```

**Error responses:**
- **400 Bad Request:** Malformed cursor.
- **401 Unauthorized:** Missing or invalid token.
- **410 Gone:** `after` cursor has been pruned.
- **503 Service Unavailable:** Database unavailable.

---

### 7.3 GET /v1/observatory/replay

**Purpose:** Fetch historical events in bounded pages (for UI replay, debugging, archival).

**Request:**
```
GET /v1/observatory/replay?after=<cursor>&through=<cursor>&limit=<n>
Authorization: Bearer <token>
Accept: application/json
```

Query parameters:
- `after` (required): Start after this cursor (exclusive).
- `through` (optional): Stop at this cursor (inclusive). Defaults to `latest_cursor`.
- `limit` (optional): Maximum events per page (default 100, max 10000).

**Response (200 OK):**
```json
{
  "schema_version": 1,
  "events": [
    {
      "schema_version": 1,
      "cursor": "1235",
      "event_id": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
      "kind": "execution.phase_changed",
      ...
    }
  ],
  "next_after": "1240",
  "has_more": true,
  "complete": false,
  "gap_before": null
}
```

**Response fields:**
- `events`: Array of EventEnvelopes in ascending cursor order.
- `next_after`: Cursor to use in the next request to continue pagination. Null if `has_more` is false.
- `has_more`: Boolean. True if more events exist after the last returned event (within `through` limit).
- `complete`: Boolean. True if a gap was detected in the requested range (e.g., crossing retention boundary). False if all events between `after` and `through` (or latest) are present.
- `gap_before`: Object (if `complete` is false). Describes the gap:
  ```json
  {
    "reason": "retention_boundary|restart",
    "at_cursor": "1200",
    "earliest_available": "1100",
    "latest_available": "1500"
  }
  ```

**Error responses:**
- **400 Bad Request:** Malformed cursor or limit.
- **401 Unauthorized:** Missing or invalid token.
- **410 Gone:** Both `after` and `through` are outside retention window.
- **503 Service Unavailable:** Database unavailable.

---

## 8. Per-Task Test Requirements

Each code task (2–9) must include tests that validate the manifest sections it implements. Tests live in `crates/ocean-observatory/tests/` or inline in source files.

### 8.1 Task 2: Ocean Observatory Crate — Event Schema, Redaction-Safe Serialization, SQLite/WAL Store, Cursor, and Retention

**Test requirements:**
- **Section 1.1 (Cursor):** `test_cursor_wire_format`, `test_cursor_overflow`, `test_cursor_monotonic_increment`.
- **Section 1.2–1.9 (Type definitions):** `test_event_envelope_validation`, `test_all_event_kinds_serialize`.
- **Section 6.2 (Redaction):** `prop_no_forbidden_fields_in_any_event` (property test with all EventKind variants).
- **Section 4 (SQLite):** `test_store_append_and_cursor_allocation`, `test_snapshot_projection_consistency`, `test_replay_with_gaps`, `test_retention_policy_enforcement`, `test_concurrent_reads_with_writes`.
- All must pass `cargo test --lib` and `cargo test --test redaction_property_tests`.

### 8.2 Task 3: Extension Admission and Host Binding Contract

**Test requirements:**
- **Section 5 (Admission):** `test_admission_request_validation`, `test_admission_depth_limit`, `test_admission_cycle_detection`, `test_admission_idempotency`, `test_cross_authority_rejection`.
- **Section 5.4 (Binding tokens):** `test_binding_token_expiry`, `test_binding_token_one_time_use`, `test_binding_token_never_persisted`.
- **Section 6.2 (Redaction):** Verify `ExecutionAdmitted` payload has no forbidden fields.

### 8.3 Task 4: Scoped Observer Auth Middleware

**Test requirements:**
- **Section 3 (Auth):** `test_observer_token_signing_hmac_sha256`, `test_observer_token_expiry_validation`, `test_observer_token_daemon_instance_binding`, `test_token_refresh_lifecycle`.
- **Section 3.3 (Scopes):** `test_scope_summary_grants_topology_access`, `test_scope_content_not_available_in_v1`, `test_unauthorized_scope_rejected`.
- **Section 3.2 (Distribution):** `test_web_client_cookie_secure_httponly_samesite`, `test_cli_client_env_var_reading`.

### 8.4 Task 5: Read-Only Observatory API Routes

**Test requirements:**
- **Section 7.1 (Snapshot):** `test_snapshot_returns_consistent_projection`, `test_snapshot_detail_parameter`, `test_snapshot_401_unauthorized`, `test_snapshot_410_retention_boundary`.
- **Section 7.2 (Tail/SSE):** `test_tail_events_stream_format`, `test_tail_resume_from_cursor`, `test_tail_gap_on_retention_boundary`, `test_tail_heartbeat_sent`, `test_tail_403_invalid_scope`.
- **Section 7.3 (Replay):** `test_replay_pagination`, `test_replay_cursor_not_found`, `test_replay_complete_flag_on_gap`, `test_replay_limit_constraints`.

### 8.5 Task 6: Wire Real Daemon Execution/Tool/Permission Facts into Observatory Pipeline

**Test requirements:**
- **Section 1.7–1.8 (EventKind variants):** Verify `ExecutionAdmitted`, `ExecutionPhaseChanged`, `ToolStarted`, `ToolFinished`, `PermissionWaiting`, `PermissionResolved` are recorded correctly.
- **Integration:** `test_real_daemon_execution_generates_events`, `test_tool_invocation_recorded_with_safe_metadata`, `test_permission_lifecycle_events`.

### 8.6 Task 7: Surface Reducer Contract Spec and Shared Test Fixtures

**Test requirements:**
- Provide fixture builders for all EventKind variants.
- `test_snapshot_fixture_deserializes`, `test_event_stream_fixture_from_json`.
- Verify fixtures conform to Section 7 API schemas.

### 8.7 Task 8: Ocean Floor Renderer Implementation Spec — Isometric Pixel-Art Scene

**Test requirements:**
- Verify renderer consumes real events from Section 7 API routes (not mock data).
- `test_renderer_integrates_snapshot_api`, `test_renderer_consumes_sse_events`, `test_renderer_no_sensitive_data_displayed`.

### 8.8 Task 9: Independent Security, Protocol, and Architecture Review

**Test requirements:**
- Verify all redaction properties hold (Section 6.2 property tests).
- Verify tokens are never persisted (Section 3.4 review).
- Verify API authorization is enforced (Section 7 all routes checked for 401/403).
- Verify extension ownership invariant is preserved (extension cannot spawn work, only attest).

---

## 9. Acceptance Criteria

This manifest gates all code tasks. A code task is not permitted to begin implementation until:

1. ✅ This manifest has been reviewed and accepted by the operator.
2. ✅ All type definitions in Section 1 are finalized.
3. ✅ All persistence contracts in Section 4 are finalized.
4. ✅ All API route contracts in Section 7 are finalized.

**Verification:**
- `cargo xtask docs-check` must pass (validates this file and all referenced spec files are coherent and reachable).
- No code changes are permitted until this gate is explicitly approved.

---

## 10. Amendment Procedure

If a task discovers an issue with this manifest (e.g., a type definition is insufficient, an API contract is ambiguous), the task **must not proceed with code**. Instead:

1. File a blocker with `pi_messenger({ action: "task.block", reason: "Manifest ambiguity: ..." })`.
2. Document the issue and propose an amendment to this file.
3. Wait for operator approval before proceeding.

---

## References

- `docs/specs/2026-07-17-observatory-gate0-decisions.md` — Gate 0 decisions and acceptance.
- `docs/specs/2026-07-17-ocean-observatory-architecture.md` — Full architecture and design rationale.
- `docs/specs/2026-07-14-ocean-extensions-architecture-and-migration-manifest.md` — Extension ownership invariant.
- `AGENTS.md` (root) — Observatory implementation is gated by this manifest.

---

## Status

**Draft:** Pending operator review and acceptance.

- [ ] Operator reviewed (Smaths)
- [ ] Operator accepted (approved to proceed with code implementation)
- [ ] All code tasks completed and tested against this manifest

