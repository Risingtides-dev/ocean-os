//! The escrow trio (OCEAN-246): a **persisted** title registry, a **Revoker**
//! principal, and a **validator escrow** ledger — the authority-in-escrow model
//! from `docs/LONGHOUSE_ORCHESTRATION.md` §2.3, made durable across turns.
//!
//! # Why this exists
//!
//! OCEAN-229 ([`crate::convene::FirekeeperTitle`]) made the firekeeper title
//! *unforgeable*: it pairs the public `agent_id` with a server-minted secret
//! token, and [`crate::convene::claim_outcome`] verifies that token in constant
//! time before honoring a `Converged`. But that title lives **only inside one
//! `convene()` stack frame** — it is minted, used, and dropped within a single
//! council. There is no daemon-held title that survives the turn, so
//! `claim_outcome` cannot be a standalone daemon op posted to between turns. That
//! limitation is called out verbatim in `longhouse_provider.rs` (the note on why
//! `claim_outcome`/`board_post` are *not* exposed as tools yet): "there is no
//! persisted, daemon-held engine to … claim an outcome against between turns".
//!
//! This module supplies the missing persistence and the two missing principals:
//!
//! 1. **[`SqliteTitleRegistry`] — authority at rest.** A durable, daemon-owned
//!    store (the "clan mother who *owns* the title") that issues, holds, and
//!    reclaims role-capability titles. A title minted in one convening can be
//!    looked up and verified in a later turn. This is what lets `claim_outcome`
//!    become a daemon-held op: the verifier persists, so a firekeeper can ratify
//!    across the turn boundary.
//! 2. **[`Revoker`] — the War Chief who *executes* deposition.** A separate
//!    principal (distinct from the registry and from every working agent) that
//!    pulls a title (graduated `Warned` strikes → hard `RoleRevoked`). A revoked
//!    title fails [`claim_outcome_persisted`] **even with the correct token**.
//!    Revocation is itself unforgeable: the Revoker holds a server-minted
//!    [`RevokerKey`], so a caller cannot forge a recall by merely naming a title.
//! 3. **[`EscrowLedger`] — validator stake held in escrow.** Validators commit a
//!    bond per topic; the bond is *held* (escrow) and *released* on a successful
//!    firekeeper claim, or *forfeited* on abort/veto. This is the bounded escrow
//!    data structure + the release-on-claim hook; the full staking *economics*
//!    (where bonds come from, slashing curves, sybil-cost calibration) is a
//!    scope-noted follow-up (see the module note on [`EscrowLedger`]).
//!
//! # Security discipline (mirrors OCEAN-185 / 220 / 229)
//!
//! * **Server-decided, unforgeable capability.** Titles and the Revoker key are
//!   minted server-side from the OS CSPRNG via [`mint_decision_token`]. A client
//!   never asserts a capability by naming a public id.
//! * **Persistence never stores the secret.** Like password storage, the registry
//!   persists a *verifier* — a per-title random salt plus `SHA-256(salt ||
//!   token)` — and **never the raw token**. A full DB dump leaks only salt+digest,
//!   from which the token cannot be recovered (preimage resistance) and a claim
//!   cannot be forged. See [`TitleVerifier`].
//! * **Constant-time comparison.** Token verification hashes the presentation and
//!   constant-time-compares the hex digests via [`decision_token_matches`] (the
//!   same OCEAN-185 primitive), so a forger cannot recover the digest by timing.
//! * **Unforgeable revocation.** A revoke requires the [`RevokerKey`]; a forged
//!   recall (no key / wrong key) is refused, so an attacker cannot deauthorize a
//!   legitimate firekeeper.
//!
//! # Storage backend — `rusqlite` (bundled), mirroring `ocean-store`
//!
//! The registry uses the same `rusqlite` (bundled) pattern as
//! [`ocean_store::SqliteRoomStore`]: synchronous, `Mutex`-guardable, hermetic
//! (no system `libsqlite3`), with `IF NOT EXISTS` idempotent migrations and
//! `IMMEDIATE` transactions on the check-then-act write paths. Time is epoch-ms
//! `i64`, matching the longhouse's existing clock discipline
//! ([`crate::convene::Clock`]) rather than dragging `chrono` in.

use ocean_core::{decision_token_matches, mint_decision_token};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::convene::{ClaimError, FirekeeperTitle};
use crate::quorum::{QuorumEngine, QuorumOutcome};

use ocean_agent_sdk::AgentRole;

/// A persistence-safe verifier for a title's secret token — the password-storage
/// analog of holding the raw token in memory.
///
/// The raw `mint_decision_token()` value is **never** persisted. Instead we store
/// a fresh per-title random `salt` and the hex `digest = SHA-256(salt || token)`.
/// Verification (see [`TitleVerifier::verifies`]) recomputes the digest over the
/// *presented* token and constant-time-compares the two hex strings. A database
/// dump therefore leaks only `(salt, digest)`, from which neither the token nor a
/// forgeable claim can be derived (SHA-256 preimage resistance).
///
/// ## Why plain salted SHA-256 (not a slow KDF like Argon2)
///
/// Slow KDFs exist to blunt brute-force over *low-entropy* secrets (human
/// passwords). The firekeeper token is ~244 bits of CSPRNG output
/// ([`mint_decision_token`] = two v4 UUIDs); brute-forcing its preimage is
/// already infeasible, so a memory-hard KDF buys nothing and would only add a
/// dependency. The salt is still included so two titles that (astronomically
/// improbably) shared a token would not share a digest, and so the digest column
/// is not a precomputation target.
#[derive(Clone, PartialEq, Eq)]
pub struct TitleVerifier {
    /// Per-title random salt (hex). Public; its only job is to de-correlate and
    /// de-precompute digests.
    salt: String,
    /// `SHA-256(salt || token)` as lowercase hex. Never the token itself.
    digest: String,
}

impl TitleVerifier {
    /// Derive a verifier from a raw secret `token`, drawing a fresh random salt.
    pub fn derive(token: &str) -> Self {
        // A 256-bit salt from the same CSPRNG primitive the tokens use.
        let salt = mint_decision_token();
        let digest = hash_token(&salt, token);
        Self { salt, digest }
    }

    /// Rebuild a verifier from already-persisted `(salt, digest)` columns.
    fn from_parts(salt: String, digest: String) -> Self {
        Self { salt, digest }
    }

    /// Constant-time check that `presented` hashes to the stored digest. The
    /// comparison is over the fixed-length hex digests via the OCEAN-185
    /// constant-time primitive, so it leaks neither length nor content by timing.
    pub fn verifies(&self, presented: &str) -> bool {
        let candidate = hash_token(&self.salt, presented);
        decision_token_matches(Some(&self.digest), Some(&candidate))
    }
}

impl std::fmt::Debug for TitleVerifier {
    /// The digest is not a secret (it is a one-way hash), but printing it invites
    /// confusion with the token; redact it and show only that a verifier exists.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TitleVerifier")
            .field("salt", &"<salt>")
            .field("digest", &"<sha256>")
            .finish()
    }
}

/// `SHA-256(salt || token)` as lowercase hex.
fn hash_token(salt: &str, token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(token.as_bytes());
    let out = hasher.finalize();
    let mut hex = String::with_capacity(out.len() * 2);
    for b in out {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// The lifecycle of a persisted title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleStatus {
    /// Held by a session, authority active. The only status under which
    /// [`claim_outcome_persisted`] can succeed.
    Live,
    /// Pulled by the [`Revoker`] (graduated demotion or hard policy breach). A
    /// revoked title can never authorize a claim again, even with the right token.
    Revoked,
    /// The topic closed and the title returned to escrow cleanly (no recall). A
    /// released title also cannot authorize a claim — its convening is over.
    Released,
}

impl TitleStatus {
    fn as_str(self) -> &'static str {
        match self {
            TitleStatus::Live => "live",
            TitleStatus::Revoked => "revoked",
            TitleStatus::Released => "released",
        }
    }
    fn parse(s: &str) -> Result<Self> {
        match s {
            "live" => Ok(TitleStatus::Live),
            "revoked" => Ok(TitleStatus::Revoked),
            "released" => Ok(TitleStatus::Released),
            other => Err(EscrowError::Encode(format!("unknown title status: {other}"))),
        }
    }
}

/// A durable, read-side view of one persisted title. Carries everything **except
/// the secret** — the raw token is never stored, only its [`TitleVerifier`], and
/// `verify` is performed inside the registry so the verifier never crosses the
/// API boundary either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedTitle {
    /// Stable id for the title grant itself (distinct from `agent_id`: the agent
    /// is the *holder*, the title is the *office*).
    pub title_id: Uuid,
    /// The topic this title is scoped to (a title is per-convening, §3).
    pub topic_id: Uuid,
    /// The public agent id bound to the title. Safe to surface; on its own it
    /// grants nothing (the OCEAN-229 lesson).
    pub agent_id: Uuid,
    /// Firekeeper or Validator — the office held.
    pub role: AgentRole,
    /// The proposal this title is bound to ratify, once known. For a firekeeper
    /// this is the engine's decision; `None` until bound.
    pub decision: Option<Uuid>,
    /// Lifecycle state. Only `Live` titles can ratify.
    pub status: TitleStatus,
    /// Soft-strike counter (graduated recall, T6). `Revoker::warn` increments it.
    pub strikes: u8,
    /// When the title was granted (epoch ms).
    pub granted_at_ms: i64,
    /// When the title was revoked/released (epoch ms), if it left `Live`.
    pub closed_at_ms: Option<i64>,
    /// Human-facing reason a title was revoked, for the audit log.
    pub revoke_reason: Option<String>,
}

/// Errors from the registry / escrow.
#[derive(Debug)]
pub enum EscrowError {
    /// No title with the given id.
    UnknownTitle(Uuid),
    /// Underlying SQLite error.
    Db(rusqlite::Error),
    /// A stored value could not be (de)serialized.
    Encode(String),
}

impl std::fmt::Display for EscrowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EscrowError::UnknownTitle(id) => write!(f, "no title with id '{id}'"),
            EscrowError::Db(e) => write!(f, "sqlite error: {e}"),
            EscrowError::Encode(e) => write!(f, "encode error: {e}"),
        }
    }
}

impl std::error::Error for EscrowError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EscrowError::Db(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for EscrowError {
    fn from(e: rusqlite::Error) -> Self {
        EscrowError::Db(e)
    }
}

type Result<T> = std::result::Result<T, EscrowError>;

fn encode_role(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Courier => "courier",
        AgentRole::Steward => "steward",
        AgentRole::Firekeeper => "firekeeper",
        AgentRole::Validator => "validator",
    }
}

fn decode_role(s: &str) -> Result<AgentRole> {
    Ok(match s {
        "courier" => AgentRole::Courier,
        "steward" => AgentRole::Steward,
        "firekeeper" => AgentRole::Firekeeper,
        "validator" => AgentRole::Validator,
        other => return Err(EscrowError::Encode(format!("unknown role: {other}"))),
    })
}

/// Parse a UUID stored as text, mapping a malformed value to an `Encode` error
/// (a corrupt row, not a panic).
fn parse_uuid(s: &str) -> Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| EscrowError::Encode(format!("bad uuid '{s}': {e}")))
}

/// The persisted, daemon-owned **Title Registry** — authority at rest (§2.3).
///
/// Issues, holds, and reclaims role-capability titles across turns. The crucial
/// property over the in-frame [`FirekeeperTitle`] is *durability*: a title minted
/// when a council converges survives the `convene()` future, so a firekeeper can
/// ratify the decision in a **later** turn via [`claim_outcome_persisted`]. The
/// registry stores a [`TitleVerifier`] (salt + hash), never the raw token, so the
/// secret is not recoverable from the database.
pub struct SqliteTitleRegistry {
    conn: Connection,
}

impl SqliteTitleRegistry {
    /// Open (or create) a registry at `path`, running migrations idempotently.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Open an in-memory registry (for tests). Migrations run on open.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Create the schema if absent. Idempotent (`IF NOT EXISTS`), so re-opening an
    /// existing DB is a no-op.
    pub fn migrate(&mut self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS titles (
                title_id      TEXT PRIMARY KEY,
                topic_id      TEXT NOT NULL,
                agent_id      TEXT NOT NULL,
                role          TEXT NOT NULL,           -- courier/steward/firekeeper/validator
                decision      TEXT,                    -- bound proposal uuid, NULL until bound
                status        TEXT NOT NULL,           -- live/revoked/released
                strikes       INTEGER NOT NULL DEFAULT 0,
                token_salt    TEXT NOT NULL,           -- per-title random salt (NOT a secret)
                token_digest  TEXT NOT NULL,           -- SHA-256(salt||token); NEVER the token
                granted_at_ms INTEGER NOT NULL,
                closed_at_ms  INTEGER,                 -- revoke/release time, NULL while live
                revoke_reason TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_titles_topic ON titles(topic_id);
            CREATE INDEX IF NOT EXISTS idx_titles_live  ON titles(topic_id, agent_id, role, status);

            -- Validator escrow: one staked bond per (topic, validator).
            CREATE TABLE IF NOT EXISTS escrow_stakes (
                topic_id     TEXT NOT NULL,
                validator_id TEXT NOT NULL,
                amount       INTEGER NOT NULL,         -- bond units held in escrow
                state        TEXT NOT NULL,            -- held/released/forfeited
                staked_at_ms INTEGER NOT NULL,
                settled_at_ms INTEGER,
                PRIMARY KEY (topic_id, validator_id)
            );

            CREATE INDEX IF NOT EXISTS idx_escrow_topic ON escrow_stakes(topic_id, state);
            "#,
        )?;
        Ok(())
    }

    /// Grant (mint + persist) a fresh title binding `agent_id` to `role` for
    /// `topic_id`. Returns the public [`PersistedTitle`] **and** the raw
    /// [`FirekeeperTitle`] holding the secret token — the caller (the convening
    /// flow / daemon) holds the token in memory to present to a claim; the
    /// registry persists only the verifier.
    ///
    /// This is the "issue a token" half of the registry's `owns` capability
    /// (`RoleGranted` is emitted by the caller, which has the event sink).
    pub fn grant(
        &mut self,
        topic_id: Uuid,
        agent_id: Uuid,
        role: AgentRole,
        now_ms: i64,
    ) -> Result<(PersistedTitle, FirekeeperTitle)> {
        // Mint the secret server-side (OCEAN-229 discipline) and derive the
        // persistence-safe verifier. The raw token leaves only inside the returned
        // FirekeeperTitle; the DB sees salt+digest.
        let secret = FirekeeperTitle::mint(agent_id);
        let verifier = TitleVerifier::derive(secret.token());
        let title_id = Uuid::new_v4();

        self.conn.execute(
            "INSERT INTO titles
               (title_id, topic_id, agent_id, role, decision, status, strikes,
                token_salt, token_digest, granted_at_ms, closed_at_ms, revoke_reason)
             VALUES (?1, ?2, ?3, ?4, NULL, 'live', 0, ?5, ?6, ?7, NULL, NULL)",
            params![
                title_id.to_string(),
                topic_id.to_string(),
                agent_id.to_string(),
                encode_role(role),
                verifier.salt,
                verifier.digest,
                now_ms,
            ],
        )?;

        let persisted = PersistedTitle {
            title_id,
            topic_id,
            agent_id,
            role,
            decision: None,
            status: TitleStatus::Live,
            strikes: 0,
            granted_at_ms: now_ms,
            closed_at_ms: None,
            revoke_reason: None,
        };
        Ok((persisted, secret))
    }

    /// Bind a `Live` title to the proposal it will ratify (the engine's decision).
    /// Idempotent on the same decision; a no-op if the title is not `Live`.
    pub fn bind_decision(&mut self, title_id: Uuid, decision: Uuid) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE titles SET decision = ?2 WHERE title_id = ?1 AND status = 'live'",
            params![title_id.to_string(), decision.to_string()],
        )?;
        if n == 0 {
            // Either unknown, or not live. Distinguish for a precise error.
            if self.lookup(title_id)?.is_none() {
                return Err(EscrowError::UnknownTitle(title_id));
            }
        }
        Ok(())
    }

    /// Look up one title by id (read-side projection; no secret).
    pub fn lookup(&self, title_id: Uuid) -> Result<Option<PersistedTitle>> {
        self.conn
            .query_row(
                "SELECT title_id, topic_id, agent_id, role, decision, status, strikes,
                        granted_at_ms, closed_at_ms, revoke_reason
                 FROM titles WHERE title_id = ?1",
                params![title_id.to_string()],
                Self::row_to_title,
            )
            .optional()
            .map_err(EscrowError::from)
            .and_then(|opt| opt.transpose())
    }

    /// The current `Live` title for a `(topic, agent, role)`, if any. Used to look
    /// a title back up in a later turn from public coordinates.
    pub fn find_live(
        &self,
        topic_id: Uuid,
        agent_id: Uuid,
        role: AgentRole,
    ) -> Result<Option<PersistedTitle>> {
        self.conn
            .query_row(
                "SELECT title_id, topic_id, agent_id, role, decision, status, strikes,
                        granted_at_ms, closed_at_ms, revoke_reason
                 FROM titles
                 WHERE topic_id = ?1 AND agent_id = ?2 AND role = ?3 AND status = 'live'",
                params![
                    topic_id.to_string(),
                    agent_id.to_string(),
                    encode_role(role)
                ],
                Self::row_to_title,
            )
            .optional()
            .map_err(EscrowError::from)
            .and_then(|opt| opt.transpose())
    }

    /// Verify a presented `(agent_id, token)` against a persisted title's identity
    /// boundary — the durable analog of [`FirekeeperTitle::authorizes`]. Returns
    /// `Ok(())` only if the title exists, is `Live` (not revoked/released), is held
    /// by `agent_id`, and `presented_token` hashes to the stored verifier.
    ///
    /// This is the persistence-backed identity half of [`claim_outcome_persisted`].
    /// A **revoked** title fails here with [`ClaimError::ForgedFirekeeper`] even
    /// when the correct token is presented — revocation deauthorizes regardless of
    /// possession of the secret. The token comparison is constant-time.
    pub fn verify_title(
        &self,
        title_id: Uuid,
        agent_id: Uuid,
        presented_token: Option<&str>,
    ) -> std::result::Result<(), ClaimError> {
        // Load the verifier columns directly (the only place the digest is read).
        let row: Option<(String, String, String, String)> = self
            .conn
            .query_row(
                "SELECT agent_id, status, token_salt, token_digest
                 FROM titles WHERE title_id = ?1",
                params![title_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(|_| ClaimError::ForgedFirekeeper)?;

        // Unknown title → treat as forged (do not leak existence vs. status).
        let Some((stored_agent, status, salt, digest)) = row else {
            return Err(ClaimError::ForgedFirekeeper);
        };

        // A non-live (revoked/released) title never authorizes — even with the
        // correct token. This is the load-bearing revocation property.
        if status != "live" {
            return Err(ClaimError::ForgedFirekeeper);
        }

        // Identity: the public id must match (namable, so `==` is fine) AND the
        // presented token must hash to the stored verifier (constant-time). We
        // compute the token check unconditionally (no `&&` short-circuit on the id)
        // so a wrong id and a wrong token are indistinguishable by timing — the id
        // is public, but keeping the verdict uniform costs nothing and mirrors
        // OCEAN-229's `FirekeeperTitle::authorizes` exactly.
        let Some(token) = presented_token else {
            return Err(ClaimError::ForgedFirekeeper);
        };
        let id_ok = stored_agent == agent_id.to_string();
        let token_ok = TitleVerifier::from_parts(salt, digest).verifies(token);
        if id_ok && token_ok {
            Ok(())
        } else {
            Err(ClaimError::ForgedFirekeeper)
        }
    }

    /// Mark a title `Released` (topic closed cleanly; title returns to escrow). A
    /// released title can no longer authorize a claim. No-op if already closed.
    pub fn release(&mut self, title_id: Uuid, now_ms: i64) -> Result<()> {
        self.close_title(title_id, TitleStatus::Released, None, now_ms)
    }

    /// Internal: transition a title out of `Live`. Used by [`release`](Self::release)
    /// and by the [`Revoker`] (revoke). Wrapped in an `IMMEDIATE` transaction so a
    /// concurrent claim verify cannot read a half-applied status.
    fn close_title(
        &mut self,
        title_id: Uuid,
        status: TitleStatus,
        reason: Option<&str>,
        now_ms: i64,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM titles WHERE title_id = ?1",
                params![title_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(EscrowError::UnknownTitle(title_id));
        }
        // Only transition a still-live title; closing an already-closed title is a
        // no-op so revoke/release are idempotent and order-independent.
        tx.execute(
            "UPDATE titles
               SET status = ?2, closed_at_ms = ?3, revoke_reason = ?4
             WHERE title_id = ?1 AND status = 'live'",
            params![title_id.to_string(), status.as_str(), now_ms, reason],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Increment the soft-strike counter (graduated recall, T6). Returns the new
    /// strike count. Only strikes a `Live` title.
    fn add_strike(&mut self, title_id: Uuid) -> Result<u8> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<i64> = tx
            .query_row(
                "SELECT strikes FROM titles WHERE title_id = ?1 AND status = 'live'",
                params![title_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            // Unknown or not live.
            if tx
                .query_row(
                    "SELECT 1 FROM titles WHERE title_id = ?1",
                    params![title_id.to_string()],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                .is_none()
            {
                return Err(EscrowError::UnknownTitle(title_id));
            }
            // Not live: strikes are meaningless on a closed title; report 0 change.
            tx.commit()?;
            return Ok(0);
        };
        let next = (current + 1).min(u8::MAX as i64);
        tx.execute(
            "UPDATE titles SET strikes = ?2 WHERE title_id = ?1",
            params![title_id.to_string(), next],
        )?;
        tx.commit()?;
        Ok(next as u8)
    }

    fn row_to_title(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<PersistedTitle>> {
        let title_id: String = row.get(0)?;
        let topic_id: String = row.get(1)?;
        let agent_id: String = row.get(2)?;
        let role: String = row.get(3)?;
        let decision: Option<String> = row.get(4)?;
        let status: String = row.get(5)?;
        let strikes: i64 = row.get(6)?;
        let granted_at_ms: i64 = row.get(7)?;
        let closed_at_ms: Option<i64> = row.get(8)?;
        let revoke_reason: Option<String> = row.get(9)?;
        Ok((|| {
            Ok(PersistedTitle {
                title_id: parse_uuid(&title_id)?,
                topic_id: parse_uuid(&topic_id)?,
                agent_id: parse_uuid(&agent_id)?,
                role: decode_role(&role)?,
                decision: decision.as_deref().map(parse_uuid).transpose()?,
                status: TitleStatus::parse(&status)?,
                strikes: strikes.clamp(0, u8::MAX as i64) as u8,
                granted_at_ms,
                closed_at_ms,
                revoke_reason,
            })
        })())
    }

    // ---- validator escrow ledger -------------------------------------------

    /// Stake a validator's bond into escrow for a topic. The bond is **held**
    /// until the topic resolves. Idempotent on `(topic, validator)`: re-staking
    /// replaces a still-`held` bond (a validator commits one bond per topic).
    pub fn stake(
        &mut self,
        topic_id: Uuid,
        validator_id: Uuid,
        amount: u64,
        now_ms: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO escrow_stakes
               (topic_id, validator_id, amount, state, staked_at_ms, settled_at_ms)
             VALUES (?1, ?2, ?3, 'held', ?4, NULL)
             ON CONFLICT(topic_id, validator_id) DO UPDATE SET
               amount = excluded.amount,
               state = 'held',
               staked_at_ms = excluded.staked_at_ms,
               settled_at_ms = NULL
             WHERE escrow_stakes.state = 'held'",
            params![
                topic_id.to_string(),
                validator_id.to_string(),
                amount as i64,
                now_ms
            ],
        )?;
        Ok(())
    }

    /// Release all `held` stakes for a topic back to their validators — the
    /// success path, called when a firekeeper claim succeeds. Returns the number
    /// of stakes released.
    fn release_stakes(&mut self, topic_id: Uuid, now_ms: i64) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE escrow_stakes
               SET state = 'released', settled_at_ms = ?2
             WHERE topic_id = ?1 AND state = 'held'",
            params![topic_id.to_string(), now_ms],
        )?;
        Ok(n)
    }

    /// Forfeit (slash) all `held` stakes for a topic — the failure path, called on
    /// abort or a sustained process veto. Returns the number forfeited.
    pub fn forfeit_stakes(&mut self, topic_id: Uuid, now_ms: i64) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE escrow_stakes
               SET state = 'forfeited', settled_at_ms = ?2
             WHERE topic_id = ?1 AND state = 'held'",
            params![topic_id.to_string(), now_ms],
        )?;
        Ok(n)
    }

    /// The current escrow state for one `(topic, validator)`, if staked.
    pub fn stake_state(
        &self,
        topic_id: Uuid,
        validator_id: Uuid,
    ) -> Result<Option<EscrowState>> {
        self.conn
            .query_row(
                "SELECT state FROM escrow_stakes WHERE topic_id = ?1 AND validator_id = ?2",
                params![topic_id.to_string(), validator_id.to_string()],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(EscrowError::from)
            .and_then(|opt| opt.map(|s| EscrowState::parse(&s)).transpose())
    }

    /// Total bond units currently `held` in escrow for a topic.
    pub fn held_total(&self, topic_id: Uuid) -> Result<u64> {
        let total: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(amount), 0) FROM escrow_stakes WHERE topic_id = ?1 AND state = 'held'",
            params![topic_id.to_string()],
            |r| r.get(0),
        )?;
        Ok(total.max(0) as u64)
    }
}

/// The escrow state of a validator's bond.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscrowState {
    /// Committed and held pending the topic's resolution.
    Held,
    /// Returned to the validator after a successful firekeeper claim.
    Released,
    /// Slashed after an abort / sustained process veto.
    Forfeited,
}

impl EscrowState {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "held" => Ok(EscrowState::Held),
            "released" => Ok(EscrowState::Released),
            "forfeited" => Ok(EscrowState::Forfeited),
            other => Err(EscrowError::Encode(format!("unknown escrow state: {other}"))),
        }
    }
}

/// The **Revoker** principal (§2.3 "War Chief") — a daemon function, distinct from
/// the registry and from every working agent, that *executes* deposition.
///
/// ## Decide ≠ execute (the constitutional split)
///
/// The design separates *the decision to revoke* from *the execution of revoke*:
/// the trigger (a quorum of `recall` marks, or a hard policy breach) is computed
/// by the daemon/[`QuorumEngine`], and the Revoker **merely executes** when that
/// daemon-checked condition is met. This type models the *execute* side. It does
/// not decide; callers pass a [`RevokeAuthorization`] that encodes the
/// already-made decision (graduated strike threshold reached, or a hard breach).
///
/// ## Unforgeable revocation
///
/// A revoke is itself a capability: the Revoker holds a server-minted
/// [`RevokerKey`] and every mutating call requires the matching key. A caller that
/// cannot present the key (a forged recall — e.g. a captured worker trying to
/// depose the firekeeper to seize the close) is refused with
/// [`RevokeError::Unauthorized`]. So just as a firekeeper cannot be *forged* into a
/// claim (OCEAN-229), a legitimate firekeeper cannot be *forcibly deauthorized* by
/// an unprivileged caller.
pub struct Revoker {
    key: RevokerKey,
}

/// A server-minted capability proving the holder may execute revocations. Minted
/// once when the daemon constructs its single [`Revoker`]; never placed on any
/// event. Holding it — not merely naming a title — is what authorizes a recall.
#[derive(Clone)]
pub struct RevokerKey(String);

impl RevokerKey {
    /// Mint a fresh Revoker capability from the OS CSPRNG.
    pub fn mint() -> Self {
        RevokerKey(mint_decision_token())
    }

    /// Constant-time check that `presented` is this key.
    fn authorizes(&self, presented: Option<&str>) -> bool {
        decision_token_matches(Some(&self.0), presented)
    }

    /// The secret, for the legitimate holder to present to its own [`Revoker`].
    pub fn secret(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for RevokerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RevokerKey(<redacted>)")
    }
}

/// Why a recall must fire — the *already-computed* decision the Revoker executes.
/// The Revoker does not evaluate these conditions (that is the daemon's job); it
/// records which path triggered the execution for the audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevokeAuthorization {
    /// Graduated path: the soft-strike threshold was reached (degraded output /
    /// repeated tool errors). Carries the strike count that tripped it.
    StrikeThreshold { strikes: u8 },
    /// Hard path: a policy breach / unsafe tool call / attempted self-renewal —
    /// immediate, no warnings ("crime in office bypasses the gradient").
    PolicyBreach { detail: String },
}

impl RevokeAuthorization {
    fn reason(&self) -> String {
        match self {
            RevokeAuthorization::StrikeThreshold { strikes } => {
                format!("graduated recall: {strikes} strikes")
            }
            RevokeAuthorization::PolicyBreach { detail } => {
                format!("hard recall: {detail}")
            }
        }
    }
}

/// The outcome of a hard revoke — enough for the caller to emit the right event
/// and (in the daemon) cancel the bound session's in-flight turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revocation {
    pub title_id: Uuid,
    pub topic_id: Uuid,
    /// The session that held the title (so the daemon can cancel its turn).
    pub agent_id: Uuid,
    /// The audit reason (for `RoleRevoked.reason`).
    pub reason: String,
}

/// Why a revoke / warn was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevokeError {
    /// The caller did not present the Revoker capability key. A forged recall.
    Unauthorized,
    /// No such title.
    UnknownTitle(Uuid),
    /// The title was not `Live` (already revoked/released) — nothing to pull.
    NotLive(Uuid),
    /// Storage failure.
    Storage(String),
}

impl std::fmt::Display for RevokeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RevokeError::Unauthorized => {
                write!(f, "revoke refused: caller did not present the Revoker key")
            }
            RevokeError::UnknownTitle(id) => write!(f, "revoke refused: no title '{id}'"),
            RevokeError::NotLive(id) => write!(f, "revoke refused: title '{id}' is not live"),
            RevokeError::Storage(e) => write!(f, "revoke storage error: {e}"),
        }
    }
}

impl std::error::Error for RevokeError {}

impl Revoker {
    /// Construct the Revoker with a freshly-minted capability key. The daemon
    /// keeps the returned key secret and presents it on every revoke; nothing else
    /// can.
    pub fn new() -> Self {
        Self {
            key: RevokerKey::mint(),
        }
    }

    /// The capability key the daemon must present to execute revocations. Exposed
    /// once at construction so the legitimate holder can call its own Revoker; a
    /// forger never receives it (it is never emitted on the wire).
    pub fn key(&self) -> RevokerKey {
        self.key.clone()
    }

    /// Execute a graduated **warn** (soft strike). Requires the Revoker key.
    /// Returns the new strike count. A caller without the key is refused
    /// ([`RevokeError::Unauthorized`]) before any state changes — a forged warn
    /// cannot accrue strikes toward a recall.
    pub fn warn(
        &self,
        registry: &mut SqliteTitleRegistry,
        presented_key: Option<&str>,
        title_id: Uuid,
        _reason: &str,
    ) -> std::result::Result<u8, RevokeError> {
        if !self.key.authorizes(presented_key) {
            return Err(RevokeError::Unauthorized);
        }
        match registry.add_strike(title_id) {
            Ok(strikes) => Ok(strikes),
            Err(EscrowError::UnknownTitle(id)) => Err(RevokeError::UnknownTitle(id)),
            Err(e) => Err(RevokeError::Storage(e.to_string())),
        }
    }

    /// Execute a **revoke** (hard pull / graduated demotion). Requires the Revoker
    /// key. Flips the title to `Revoked` so it can never authorize a claim again,
    /// and returns a [`Revocation`] the caller uses to emit `RoleRevoked` and (in
    /// the daemon) cancel the bound session's turn.
    ///
    /// Authorization is checked **first**: a forged revoke (no/invalid key) is
    /// refused before the registry is touched, so an unprivileged caller cannot
    /// deauthorize a legitimate firekeeper, nor learn whether the title exists.
    pub fn revoke(
        &self,
        registry: &mut SqliteTitleRegistry,
        presented_key: Option<&str>,
        title_id: Uuid,
        authorization: RevokeAuthorization,
        now_ms: i64,
    ) -> std::result::Result<Revocation, RevokeError> {
        if !self.key.authorizes(presented_key) {
            return Err(RevokeError::Unauthorized);
        }

        // Snapshot the (public) title to build the Revocation and confirm it is
        // live before pulling it.
        let title = registry
            .lookup(title_id)
            .map_err(|e| RevokeError::Storage(e.to_string()))?
            .ok_or(RevokeError::UnknownTitle(title_id))?;
        if title.status != TitleStatus::Live {
            return Err(RevokeError::NotLive(title_id));
        }

        let reason = authorization.reason();
        registry
            .close_title(title_id, TitleStatus::Revoked, Some(&reason), now_ms)
            .map_err(|e| match e {
                EscrowError::UnknownTitle(id) => RevokeError::UnknownTitle(id),
                other => RevokeError::Storage(other.to_string()),
            })?;

        Ok(Revocation {
            title_id,
            topic_id: title.topic_id,
            agent_id: title.agent_id,
            reason,
        })
    }
}

impl Default for Revoker {
    fn default() -> Self {
        Self::new()
    }
}

/// The **daemon-held** `claim_outcome` — the persisted analog of
/// [`crate::convene::claim_outcome`], the op `longhouse_provider.rs` notes cannot
/// exist without a persisted title.
///
/// It guards the same two boundaries as the in-frame gate, but the identity
/// boundary is satisfied from the **persisted** registry (so a firekeeper can
/// ratify in a *later* turn than the one that minted the title), and it adds the
/// revocation boundary:
///
/// 1. **Persisted identity + not revoked (OCEAN-246 / 229).** The registry
///    verifies the presented `(agent_id, token)` against the stored verifier in
///    constant time, **and** requires the title to still be `Live`. A revoked or
///    released title fails here — even with the correct token — so a recalled
///    firekeeper cannot ratify. Checked **first**, before the engine, so the
///    refusal leaks no engine state.
/// 2. **Engine agreement (the original brake).** Reuses
///    [`crate::convene::claim_outcome`] semantics: the engine must report
///    `Converged` on exactly `claimed`. Premature → `NotConverged`; wrong proposal
///    → `WrongDecision`.
///
/// On success, the validator escrow for the topic is **released** (the bonds did
/// their job: a legitimate, quorum-backed close happened). The release is
/// best-effort with respect to its own storage error, which is surfaced via
/// `escrow_released`.
///
/// Returns the number of validator stakes released on success.
pub fn claim_outcome_persisted(
    registry: &mut SqliteTitleRegistry,
    engine: &mut QuorumEngine,
    title_id: Uuid,
    agent_id: Uuid,
    presented_token: Option<&str>,
    claimed: Uuid,
    now_ms: i64,
) -> std::result::Result<usize, ClaimError> {
    // Boundary 1 — persisted identity + liveness (rejects revoked/released).
    registry.verify_title(title_id, agent_id, presented_token)?;

    // Boundary 2 — the firekeeper may only ratify the engine's own decision.
    match engine.evaluate(now_ms) {
        QuorumOutcome::Converged { decision, .. } => {
            if decision != claimed {
                return Err(ClaimError::WrongDecision {
                    engine_decision: decision,
                    claimed,
                });
            }
        }
        QuorumOutcome::Pending { .. } => return Err(ClaimError::NotConverged),
    }

    // Authorized + ratified: release the title (its convening is closing) and the
    // validator escrow for this topic. A storage hiccup here does not retroactively
    // un-authorize the claim — the claim already succeeded — so we ignore the
    // release error for the gate's verdict but still attempt both.
    let topic_id = registry.lookup(title_id).ok().flatten().map(|t| t.topic_id);
    let _ = registry.release(title_id, now_ms);
    let released = match topic_id {
        Some(topic) => registry.release_stakes(topic, now_ms).unwrap_or(0),
        None => 0,
    };
    Ok(released)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quorum::{QuorumConfig, QuorumRule};

    fn uid(n: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[15] = n;
        Uuid::from_bytes(b)
    }

    fn fast_quorum() -> QuorumConfig {
        QuorumConfig {
            rule: QuorumRule::NetWeight {
                cutoff: 2.0,
                margin: 1.0,
            },
            mark_ttl_ms: 60_000,
            tie_break_seed: 1,
        }
    }

    /// Drive an engine to genuine convergence on `proposal`.
    fn converge_on(proposal: Uuid, a: Uuid, b: Uuid) -> QuorumEngine {
        let mut eng = QuorumEngine::new(fast_quorum());
        eng.propose(proposal, a, 0);
        eng.endorse(proposal, b, None, 0);
        assert!(matches!(eng.evaluate(0), QuorumOutcome::Converged { .. }));
        eng
    }

    // ---- TitleVerifier: persistence without leaking the secret --------------

    #[test]
    fn verifier_accepts_right_token_rejects_wrong() {
        let token = mint_decision_token();
        let v = TitleVerifier::derive(&token);
        assert!(v.verifies(&token), "right token must verify");
        assert!(!v.verifies("nope"), "wrong token must not verify");
        assert!(
            !v.verifies(&mint_decision_token()),
            "an independently-minted token must not verify"
        );
    }

    #[test]
    fn verifier_never_contains_the_raw_token() {
        // The persistence-safe property: neither salt nor digest is the token, and
        // Debug redacts both. A DB dump (salt+digest) cannot recover the token.
        let token = mint_decision_token();
        let v = TitleVerifier::derive(&token);
        assert_ne!(v.salt, token, "salt must not be the token");
        assert_ne!(v.digest, token, "digest must not be the token");
        let shown = format!("{v:?}");
        assert!(!shown.contains(&token), "Debug must not print the token");
    }

    #[test]
    fn distinct_salts_give_distinct_digests_for_same_token() {
        // Two titles deriving from the same token must not share a digest (salt
        // de-correlates them, defeating precomputation/rainbow tables).
        let token = mint_decision_token();
        let a = TitleVerifier::derive(&token);
        let b = TitleVerifier::derive(&token);
        assert_ne!(a.salt, b.salt);
        assert_ne!(a.digest, b.digest);
    }

    // ---- (a) Persisted TitleRegistry: titles survive turns ------------------

    // THE CORE OCEAN-246 PROPERTY. A title minted in one "turn" (one registry
    // call) is looked up and verified in a LATER turn — modeled by closing the
    // registry's process and re-opening the same DB file. This is what makes
    // claim_outcome a daemon-held op.
    #[test]
    fn title_persists_and_verifies_across_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("titles.db");
        let topic = uid(1);
        let agent = uid(10);

        // Turn 1: grant a firekeeper title; capture the secret the daemon holds.
        let (title_id, secret_token) = {
            let mut reg = SqliteTitleRegistry::open(&path).unwrap();
            let (persisted, secret) = reg.grant(topic, agent, AgentRole::Firekeeper, 0).unwrap();
            assert_eq!(persisted.status, TitleStatus::Live);
            (persisted.title_id, secret.token().to_string())
        };

        // Turn 2 (fresh process): re-open the SAME db. The title is still there,
        // and the persisted verifier authorizes the original secret token.
        let reg = SqliteTitleRegistry::open(&path).unwrap();
        let found = reg.lookup(title_id).unwrap().expect("title survived reopen");
        assert_eq!(found.agent_id, agent);
        assert_eq!(found.role, AgentRole::Firekeeper);
        assert_eq!(found.status, TitleStatus::Live);

        // The right token verifies across the turn boundary; a wrong one does not.
        assert_eq!(reg.verify_title(title_id, agent, Some(&secret_token)), Ok(()));
        assert_eq!(
            reg.verify_title(title_id, agent, Some("forged")),
            Err(ClaimError::ForgedFirekeeper)
        );
    }

    #[test]
    fn find_live_recovers_title_from_public_coordinates() {
        let mut reg = SqliteTitleRegistry::open_in_memory().unwrap();
        let topic = uid(1);
        let agent = uid(10);
        let (persisted, _secret) = reg.grant(topic, agent, AgentRole::Firekeeper, 0).unwrap();
        let found = reg
            .find_live(topic, agent, AgentRole::Firekeeper)
            .unwrap()
            .expect("live title found by coordinates");
        assert_eq!(found.title_id, persisted.title_id);
        // Wrong role / agent → no match.
        assert!(reg
            .find_live(topic, agent, AgentRole::Validator)
            .unwrap()
            .is_none());
        assert!(reg
            .find_live(topic, uid(99), AgentRole::Firekeeper)
            .unwrap()
            .is_none());
    }

    #[test]
    fn db_dump_contains_only_verifier_never_the_token() {
        // Persistence-without-leaking, at the storage layer: read the raw columns
        // and assert the token is absent and only salt+digest are stored.
        let mut reg = SqliteTitleRegistry::open_in_memory().unwrap();
        let (persisted, secret) = reg.grant(uid(1), uid(10), AgentRole::Firekeeper, 0).unwrap();
        let (salt, digest): (String, String) = reg
            .conn
            .query_row(
                "SELECT token_salt, token_digest FROM titles WHERE title_id = ?1",
                params![persisted.title_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_ne!(salt, secret.token());
        assert_ne!(digest, secret.token());
        // And the digest is exactly SHA-256(salt || token) — verifiable, one-way.
        assert_eq!(digest, hash_token(&salt, secret.token()));
    }

    #[test]
    fn migrate_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("titles.db");
        let mut reg = SqliteTitleRegistry::open(&path).unwrap();
        let (p, _s) = reg.grant(uid(1), uid(10), AgentRole::Firekeeper, 0).unwrap();
        reg.migrate().unwrap();
        reg.migrate().unwrap();
        assert!(reg.lookup(p.title_id).unwrap().is_some());
    }

    // ---- (b) Revoker: graduated recall + unforgeable revocation -------------

    // A revoked title fails claim_outcome_persisted EVEN WITH THE RIGHT TOKEN.
    #[test]
    fn revoked_title_is_rejected_by_claim_even_with_right_token() {
        let mut reg = SqliteTitleRegistry::open_in_memory().unwrap();
        let topic = uid(1);
        let proposal = uid(2);
        let (a, b) = (uid(10), uid(11));
        let (title, secret) = reg.grant(topic, a, AgentRole::Firekeeper, 0).unwrap();
        let eng = converge_on(proposal, a, b);

        // Sanity: before revocation the persisted identity boundary passes for the
        // correct token (proving the rejection below is *caused* by revocation,
        // not a bad token).
        assert_eq!(
            reg.verify_title(title.title_id, a, Some(secret.token())),
            Ok(())
        );

        // Now revoke via the Revoker (with its key), then a claim with the correct
        // token must be refused as forged (the title is no longer live).
        let mut eng = eng;
        let revoker = Revoker::new();
        let key = revoker.key();
        let rev = revoker
            .revoke(
                &mut reg,
                Some(key.secret()),
                title.title_id,
                RevokeAuthorization::PolicyBreach {
                    detail: "unsafe tool call".into(),
                },
                5,
            )
            .expect("authorized revoke succeeds");
        assert_eq!(rev.agent_id, a);
        assert_eq!(rev.topic_id, topic);

        let result = claim_outcome_persisted(
            &mut reg,
            &mut eng,
            title.title_id,
            a,
            Some(secret.token()),
            proposal,
            0,
        );
        assert_eq!(
            result,
            Err(ClaimError::ForgedFirekeeper),
            "a revoked title must be rejected even with the correct token"
        );
        // Status really is revoked, with the audit reason.
        let after = reg.lookup(title.title_id).unwrap().unwrap();
        assert_eq!(after.status, TitleStatus::Revoked);
        assert!(after.revoke_reason.unwrap().contains("hard recall"));
    }

    // A FORGED revocation (no/invalid Revoker key) is refused — a legitimate
    // firekeeper cannot be deauthorized by an unprivileged caller.
    #[test]
    fn forged_revocation_is_refused() {
        let mut reg = SqliteTitleRegistry::open_in_memory().unwrap();
        let topic = uid(1);
        let a = uid(10);
        let (title, _secret) = reg.grant(topic, a, AgentRole::Firekeeper, 0).unwrap();

        let revoker = Revoker::new();
        // No key presented.
        assert_eq!(
            revoker.revoke(
                &mut reg,
                None,
                title.title_id,
                RevokeAuthorization::PolicyBreach { detail: "x".into() },
                0
            ),
            Err(RevokeError::Unauthorized)
        );
        // Wrong key (an independently-minted token, what an attacker could make).
        let attacker = mint_decision_token();
        assert_eq!(
            revoker.revoke(
                &mut reg,
                Some(&attacker),
                title.title_id,
                RevokeAuthorization::PolicyBreach { detail: "x".into() },
                0
            ),
            Err(RevokeError::Unauthorized)
        );
        // The title is untouched: still live.
        assert_eq!(
            reg.lookup(title.title_id).unwrap().unwrap().status,
            TitleStatus::Live
        );
    }

    #[test]
    fn graduated_warn_accrues_strikes_then_hard_revoke() {
        let mut reg = SqliteTitleRegistry::open_in_memory().unwrap();
        let topic = uid(1);
        let a = uid(10);
        let (title, _s) = reg.grant(topic, a, AgentRole::Firekeeper, 0).unwrap();
        let revoker = Revoker::new();
        let key = revoker.key();

        // Soft strikes accrue (graduated path).
        assert_eq!(
            revoker.warn(&mut reg, Some(key.secret()), title.title_id, "degraded").unwrap(),
            1
        );
        assert_eq!(
            revoker.warn(&mut reg, Some(key.secret()), title.title_id, "tool error").unwrap(),
            2
        );
        assert_eq!(reg.lookup(title.title_id).unwrap().unwrap().strikes, 2);

        // A forged warn cannot accrue strikes.
        assert_eq!(
            revoker.warn(&mut reg, None, title.title_id, "spoof"),
            Err(RevokeError::Unauthorized)
        );
        assert_eq!(reg.lookup(title.title_id).unwrap().unwrap().strikes, 2);

        // At threshold, the Revoker demotes (graduated → hard).
        revoker
            .revoke(
                &mut reg,
                Some(key.secret()),
                title.title_id,
                RevokeAuthorization::StrikeThreshold { strikes: 2 },
                10,
            )
            .unwrap();
        let after = reg.lookup(title.title_id).unwrap().unwrap();
        assert_eq!(after.status, TitleStatus::Revoked);
        assert!(after.revoke_reason.unwrap().contains("graduated recall"));
    }

    #[test]
    fn revoke_is_idempotent_and_rejects_double_pull() {
        let mut reg = SqliteTitleRegistry::open_in_memory().unwrap();
        let a = uid(10);
        let (title, _s) = reg.grant(uid(1), a, AgentRole::Firekeeper, 0).unwrap();
        let revoker = Revoker::new();
        let key = revoker.key();
        revoker
            .revoke(
                &mut reg,
                Some(key.secret()),
                title.title_id,
                RevokeAuthorization::PolicyBreach { detail: "x".into() },
                0,
            )
            .unwrap();
        // Second revoke of an already-revoked title is refused as NotLive.
        assert_eq!(
            revoker.revoke(
                &mut reg,
                Some(key.secret()),
                title.title_id,
                RevokeAuthorization::PolicyBreach { detail: "x".into() },
                1
            ),
            Err(RevokeError::NotLive(title.title_id))
        );
    }

    // ---- (c) Validator escrow: hold → release / forfeit ---------------------

    #[test]
    fn escrow_holds_then_releases_on_successful_claim() {
        let mut reg = SqliteTitleRegistry::open_in_memory().unwrap();
        let topic = uid(1);
        let proposal = uid(2);
        let (fk_a, fk_b) = (uid(10), uid(11));
        let (v1, v2) = (uid(20), uid(21));

        // Two validators stake into escrow; bonds are held.
        reg.stake(topic, v1, 100, 0).unwrap();
        reg.stake(topic, v2, 50, 0).unwrap();
        assert_eq!(reg.held_total(topic).unwrap(), 150);
        assert_eq!(reg.stake_state(topic, v1).unwrap(), Some(EscrowState::Held));

        // The firekeeper claims successfully → escrow releases.
        let (title, secret) = reg.grant(topic, fk_a, AgentRole::Firekeeper, 0).unwrap();
        let mut eng = converge_on(proposal, fk_a, fk_b);
        let released = claim_outcome_persisted(
            &mut reg,
            &mut eng,
            title.title_id,
            fk_a,
            Some(secret.token()),
            proposal,
            0,
        )
        .expect("legit claim succeeds");
        assert_eq!(released, 2, "both validator stakes released");
        assert_eq!(reg.stake_state(topic, v1).unwrap(), Some(EscrowState::Released));
        assert_eq!(reg.stake_state(topic, v2).unwrap(), Some(EscrowState::Released));
        assert_eq!(reg.held_total(topic).unwrap(), 0);
        // And the title was released as part of the close.
        assert_eq!(
            reg.lookup(title.title_id).unwrap().unwrap().status,
            TitleStatus::Released
        );
    }

    #[test]
    fn escrow_forfeits_on_abort() {
        let mut reg = SqliteTitleRegistry::open_in_memory().unwrap();
        let topic = uid(1);
        let v1 = uid(20);
        reg.stake(topic, v1, 100, 0).unwrap();
        assert_eq!(reg.stake_state(topic, v1).unwrap(), Some(EscrowState::Held));
        let n = reg.forfeit_stakes(topic, 5).unwrap();
        assert_eq!(n, 1);
        assert_eq!(reg.stake_state(topic, v1).unwrap(), Some(EscrowState::Forfeited));
        assert_eq!(reg.held_total(topic).unwrap(), 0);
    }

    #[test]
    fn failed_claim_does_not_release_escrow() {
        // A premature claim (engine still pending) is rejected AND must leave the
        // escrow held — bonds are only released on a genuine, ratified close.
        let mut reg = SqliteTitleRegistry::open_in_memory().unwrap();
        let topic = uid(1);
        let proposal = uid(2);
        let (fk, v1) = (uid(10), uid(20));
        reg.stake(topic, v1, 100, 0).unwrap();
        let (title, secret) = reg.grant(topic, fk, AgentRole::Firekeeper, 0).unwrap();

        // Engine pending (only the implicit self-endorse).
        let mut eng = QuorumEngine::new(fast_quorum());
        eng.propose(proposal, fk, 0);
        assert!(matches!(eng.evaluate(0), QuorumOutcome::Pending { .. }));

        let result = claim_outcome_persisted(
            &mut reg,
            &mut eng,
            title.title_id,
            fk,
            Some(secret.token()),
            proposal,
            0,
        );
        assert_eq!(result, Err(ClaimError::NotConverged));
        // Escrow untouched; title still live.
        assert_eq!(reg.stake_state(topic, v1).unwrap(), Some(EscrowState::Held));
        assert_eq!(
            reg.lookup(title.title_id).unwrap().unwrap().status,
            TitleStatus::Live
        );
    }

    // ---- persisted claim_outcome: full identity + engine boundaries ---------

    #[test]
    fn persisted_claim_accepts_legit_firekeeper_with_token() {
        let mut reg = SqliteTitleRegistry::open_in_memory().unwrap();
        let topic = uid(1);
        let proposal = uid(2);
        let (a, b) = (uid(10), uid(11));
        let (title, secret) = reg.grant(topic, a, AgentRole::Firekeeper, 0).unwrap();
        let mut eng = converge_on(proposal, a, b);
        assert_eq!(
            claim_outcome_persisted(
                &mut reg,
                &mut eng,
                title.title_id,
                a,
                Some(secret.token()),
                proposal,
                0
            ),
            Ok(0) // no validators staked → 0 released, but the claim is authorized
        );
    }

    #[test]
    fn persisted_claim_rejects_forged_no_token() {
        let mut reg = SqliteTitleRegistry::open_in_memory().unwrap();
        let topic = uid(1);
        let proposal = uid(2);
        let (a, b) = (uid(10), uid(11));
        let (title, _secret) = reg.grant(topic, a, AgentRole::Firekeeper, 0).unwrap();
        let mut eng = converge_on(proposal, a, b);
        // No token presented → forged, even though quorum converged.
        assert_eq!(
            claim_outcome_persisted(&mut reg, &mut eng, title.title_id, a, None, proposal, 0),
            Err(ClaimError::ForgedFirekeeper)
        );
    }

    #[test]
    fn persisted_claim_rejects_real_token_wrong_id() {
        let mut reg = SqliteTitleRegistry::open_in_memory().unwrap();
        let topic = uid(1);
        let proposal = uid(2);
        let (a, b, imposter) = (uid(10), uid(11), uid(99));
        let (title, secret) = reg.grant(topic, a, AgentRole::Firekeeper, 0).unwrap();
        let mut eng = converge_on(proposal, a, b);
        // The real token, but asserted under the wrong agent id → forged.
        assert_eq!(
            claim_outcome_persisted(
                &mut reg,
                &mut eng,
                title.title_id,
                imposter,
                Some(secret.token()),
                proposal,
                0
            ),
            Err(ClaimError::ForgedFirekeeper)
        );
    }

    #[test]
    fn persisted_claim_rejects_wrong_decision() {
        let mut reg = SqliteTitleRegistry::open_in_memory().unwrap();
        let topic = uid(1);
        let (winner, other) = (uid(2), uid(3));
        let (a, b) = (uid(10), uid(11));
        let (title, secret) = reg.grant(topic, a, AgentRole::Firekeeper, 0).unwrap();
        let mut eng = converge_on(winner, a, b);
        eng.propose(other, uid(20), 0); // a rival that did not win
        assert_eq!(
            claim_outcome_persisted(
                &mut reg,
                &mut eng,
                title.title_id,
                a,
                Some(secret.token()),
                other,
                0
            ),
            Err(ClaimError::WrongDecision {
                engine_decision: winner,
                claimed: other,
            })
        );
        // A rejected claim must not have released the title (still live to retry).
        assert_eq!(
            reg.lookup(title.title_id).unwrap().unwrap().status,
            TitleStatus::Live
        );
    }

    #[test]
    fn persisted_claim_identity_checked_before_convergence() {
        // A forged claim while the engine is still pending must surface
        // ForgedFirekeeper (identity first), not NotConverged — no engine-state leak.
        let mut reg = SqliteTitleRegistry::open_in_memory().unwrap();
        let topic = uid(1);
        let proposal = uid(2);
        let a = uid(10);
        let (title, _secret) = reg.grant(topic, a, AgentRole::Firekeeper, 0).unwrap();
        let mut eng = QuorumEngine::new(fast_quorum());
        eng.propose(proposal, a, 0); // Pending
        assert!(matches!(eng.evaluate(0), QuorumOutcome::Pending { .. }));
        assert_eq!(
            claim_outcome_persisted(&mut reg, &mut eng, title.title_id, a, None, proposal, 0),
            Err(ClaimError::ForgedFirekeeper)
        );
    }

    #[test]
    fn unknown_title_verifies_as_forged_not_panics() {
        let reg = SqliteTitleRegistry::open_in_memory().unwrap();
        assert_eq!(
            reg.verify_title(uid(123), uid(10), Some("whatever")),
            Err(ClaimError::ForgedFirekeeper)
        );
    }
}
