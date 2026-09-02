//! In-process turn metrics and Prometheus text rendering.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ocean_core::RoomAccessState;

/// Upper bounds (inclusive, milliseconds) of the turn-latency histogram buckets.
/// A turn that took `wall_ms` increments every bucket whose bound it is `<=`,
/// matching Prometheus cumulative-histogram semantics (`le` = "less than or
/// equal"). The implicit `+Inf` bucket is the total turn count and is emitted
/// separately. Chosen to span sub-second turns through multi-minute agent loops:
/// 50ms / 100ms / 250ms / 500ms / 1s / 2.5s / 5s / 10s / 30s / 60s / 120s.
const TURN_LATENCY_BUCKETS_MS: [u64; 11] = [
    50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 120_000,
];

const ADVISOR_LATENCY_BUCKETS_MS: [u64; 8] = [10, 50, 100, 250, 500, 1_000, 5_000, 30_000];

#[derive(Clone, Copy, Debug)]
pub(super) enum AdvisorOutcome {
    Emitted,
    Suppressed,
    ProviderError,
    Timeout,
    Saturated,
}

impl AdvisorOutcome {
    const ALL: [Self; 5] = [
        Self::Emitted,
        Self::Suppressed,
        Self::ProviderError,
        Self::Timeout,
        Self::Saturated,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Emitted => 0,
            Self::Suppressed => 1,
            Self::ProviderError => 2,
            Self::Timeout => 3,
            Self::Saturated => 4,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Emitted => "emitted",
            Self::Suppressed => "suppressed",
            Self::ProviderError => "provider_error",
            Self::Timeout => "timeout",
            Self::Saturated => "saturated",
        }
    }
}

/// Daemon-wide turn metrics (OCEAN-303). Every field is a relaxed `AtomicU64`;
/// reads (the `/metrics` render) and writes (the turn hot path) are all
/// `Ordering::Relaxed` because these are independent monotonic counters / a
/// gauge — there is no cross-counter invariant that needs ordering, only
/// eventual per-counter accuracy. Default-constructs to all-zero, which renders
/// as a valid (empty) metrics surface before the first turn.
#[derive(Default)]
pub(super) struct TurnMetrics {
    /// Turns currently executing (`runtime.prompt` in flight). Incremented when a
    /// turn starts and decremented when it finishes, via the RAII
    /// [`InFlightGuard`] so it is balanced even if the turn task panics or is
    /// cancelled. This is a gauge, not a counter — it goes both up and down.
    in_flight: std::sync::atomic::AtomicU64,
    /// Turns that finished with `ok == true` (the runtime returned a successful
    /// `PromptResponse`). Monotonic.
    turns_ok: std::sync::atomic::AtomicU64,
    /// Turns that finished with `ok == false` (the runtime returned an errored
    /// `PromptResponse` — provider failure after failover was exhausted, a tool
    /// hard-error, a cancelled run, etc.). This is the daemon-observable
    /// turn-failure signal; the provider-failover machinery (OCEAN-275) lives in
    /// `ocean-agent` and surfaces its outcome to the daemon only as `res.ok`.
    /// Monotonic.
    turns_error: std::sync::atomic::AtomicU64,
    /// Cumulative count of turns whose `wall_ms` was `<=` the bucket bound at the
    /// same index in [`TURN_LATENCY_BUCKETS_MS`]. Prometheus histogram buckets
    /// are cumulative, so a single turn bumps every bucket it falls under.
    latency_buckets: [std::sync::atomic::AtomicU64; TURN_LATENCY_BUCKETS_MS.len()],
    /// Sum of every finished turn's `wall_ms`, in milliseconds. The `_sum`
    /// companion of a Prometheus histogram; with the `+Inf` count it yields the
    /// average turn latency.
    latency_sum_ms: std::sync::atomic::AtomicU64,
    /// Advisor provider calls currently executing. Independent of turn in-flight.
    advisor_in_flight: std::sync::atomic::AtomicU64,
    /// Fixed-cardinality advisor terminal outcomes. The index is owned by
    /// [`AdvisorOutcome`]; no request, session, model, or content is a label.
    advisor_outcomes: [std::sync::atomic::AtomicU64; AdvisorOutcome::ALL.len()],
    /// Cumulative advisor attempt latency, including immediate saturation.
    advisor_latency_buckets: [std::sync::atomic::AtomicU64; ADVISOR_LATENCY_BUCKETS_MS.len()],
    advisor_latency_sum_ms: std::sync::atomic::AtomicU64,
}

impl TurnMetrics {
    /// Record a finished turn: fold `wall_ms` into the latency histogram + sum and
    /// bump the ok/error counter. Pure relaxed `fetch_add`s — cheap enough to sit
    /// directly on the turn-finish path. Called once per turn, right where
    /// `wall_ms`/`ok` are already computed for the "agent turn finished" log.
    pub(super) fn record_turn(&self, wall_ms: u64, ok: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        if ok {
            self.turns_ok.fetch_add(1, Relaxed);
        } else {
            self.turns_error.fetch_add(1, Relaxed);
        }
        self.latency_sum_ms.fetch_add(wall_ms, Relaxed);
        for (bound, bucket) in TURN_LATENCY_BUCKETS_MS
            .iter()
            .zip(self.latency_buckets.iter())
        {
            if wall_ms <= *bound {
                bucket.fetch_add(1, Relaxed);
            }
        }
    }

    pub(super) fn record_advisor(&self, outcome: AdvisorOutcome, elapsed: std::time::Duration) {
        use std::sync::atomic::Ordering::Relaxed;
        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        self.advisor_outcomes[outcome.index()].fetch_add(1, Relaxed);
        self.advisor_latency_sum_ms.fetch_add(elapsed_ms, Relaxed);
        for (bound, bucket) in ADVISOR_LATENCY_BUCKETS_MS
            .iter()
            .zip(self.advisor_latency_buckets.iter())
        {
            if elapsed_ms <= *bound {
                bucket.fetch_add(1, Relaxed);
            }
        }
    }

    /// Render the full Prometheus text-format exposition (v0.0.4) for this set of
    /// counters plus the externally-owned `persist_failures`, `gc_failures`, and
    /// SSE-lag gauges. Read-only over relaxed atomics, so scraping never perturbs
    /// the hot path. Each metric gets its `# HELP`/`# TYPE` header per the
    /// exposition format.
    pub(super) fn render_prometheus(
        &self,
        persist_failures: u64,
        gc_failures: u64,
        sse_lag_events: u64,
        sse_events_dropped: u64,
    ) -> String {
        use std::fmt::Write as _;
        use std::sync::atomic::Ordering::Relaxed;

        let ok = self.turns_ok.load(Relaxed);
        let error = self.turns_error.load(Relaxed);
        let total = ok + error;
        let sum_ms = self.latency_sum_ms.load(Relaxed);
        let in_flight = self.in_flight.load(Relaxed);
        let advisor_in_flight = self.advisor_in_flight.load(Relaxed);
        let advisor_total = self
            .advisor_outcomes
            .iter()
            .map(|counter| counter.load(Relaxed))
            .sum::<u64>();

        let mut out = String::with_capacity(2048);

        // Turn count by outcome (labelled counter).
        out.push_str("# HELP ocean_turns_total Total agent turns finished, by outcome.\n");
        out.push_str("# TYPE ocean_turns_total counter\n");
        let _ = writeln!(out, "ocean_turns_total{{outcome=\"ok\"}} {ok}");
        let _ = writeln!(out, "ocean_turns_total{{outcome=\"error\"}} {error}");

        // In-flight turns (gauge).
        out.push_str("# HELP ocean_turns_in_flight Agent turns currently executing.\n");
        out.push_str("# TYPE ocean_turns_in_flight gauge\n");
        let _ = writeln!(out, "ocean_turns_in_flight {in_flight}");

        // Turn-latency histogram. Buckets are cumulative; the `+Inf` bucket equals
        // the total turn count, and `_sum` is in seconds (Prometheus convention is
        // base units — we measure in ms, so divide by 1000 for the seconds view).
        out.push_str(
            "# HELP ocean_turn_duration_seconds Agent turn wall-clock duration in seconds.\n",
        );
        out.push_str("# TYPE ocean_turn_duration_seconds histogram\n");
        for (bound_ms, bucket) in TURN_LATENCY_BUCKETS_MS
            .iter()
            .zip(self.latency_buckets.iter())
        {
            let count = bucket.load(Relaxed);
            let le_seconds = (*bound_ms as f64) / 1000.0;
            let _ = writeln!(
                out,
                "ocean_turn_duration_seconds_bucket{{le=\"{le_seconds}\"}} {count}"
            );
        }
        let _ = writeln!(
            out,
            "ocean_turn_duration_seconds_bucket{{le=\"+Inf\"}} {total}"
        );
        let sum_seconds = (sum_ms as f64) / 1000.0;
        let _ = writeln!(out, "ocean_turn_duration_seconds_sum {sum_seconds}");
        let _ = writeln!(out, "ocean_turn_duration_seconds_count {total}");

        out.push_str(
            "# HELP ocean_advisor_in_flight Advisor provider calls currently executing.\n",
        );
        out.push_str("# TYPE ocean_advisor_in_flight gauge\n");
        let _ = writeln!(out, "ocean_advisor_in_flight {advisor_in_flight}");

        out.push_str(
            "# HELP ocean_advisor_outcomes_total Post-turn advisor attempts by terminal outcome.\n",
        );
        out.push_str("# TYPE ocean_advisor_outcomes_total counter\n");
        for outcome in AdvisorOutcome::ALL {
            let count = self.advisor_outcomes[outcome.index()].load(Relaxed);
            let _ = writeln!(
                out,
                "ocean_advisor_outcomes_total{{outcome=\"{}\"}} {count}",
                outcome.label()
            );
        }

        out.push_str("# HELP ocean_advisor_duration_seconds Post-turn advisor attempt duration in seconds.\n");
        out.push_str("# TYPE ocean_advisor_duration_seconds histogram\n");
        for (bound_ms, bucket) in ADVISOR_LATENCY_BUCKETS_MS
            .iter()
            .zip(self.advisor_latency_buckets.iter())
        {
            let count = bucket.load(Relaxed);
            let le_seconds = (*bound_ms as f64) / 1000.0;
            let _ = writeln!(
                out,
                "ocean_advisor_duration_seconds_bucket{{le=\"{le_seconds}\"}} {count}"
            );
        }
        let _ = writeln!(
            out,
            "ocean_advisor_duration_seconds_bucket{{le=\"+Inf\"}} {advisor_total}"
        );
        let advisor_sum_seconds = (self.advisor_latency_sum_ms.load(Relaxed) as f64) / 1000.0;
        let _ = writeln!(
            out,
            "ocean_advisor_duration_seconds_sum {advisor_sum_seconds}"
        );
        let _ = writeln!(out, "ocean_advisor_duration_seconds_count {advisor_total}");

        // Dropped-transcript-write count (OCEAN-255), read from the single source
        // of truth on `AppState`. Mirrors what `GET /health` reports.
        out.push_str(
            "# HELP ocean_persist_failures_total Call-transcript writes dropped after retry.\n",
        );
        out.push_str("# TYPE ocean_persist_failures_total counter\n");
        let _ = writeln!(out, "ocean_persist_failures_total {persist_failures}");

        // Failed-registry-GC-sweep count (OCEAN-371), read from the single source of
        // truth on `AppState`. Mirrors what `GET /health` reports as
        // `gc_failures_total`. A climbing value means the background GC loop is
        // failing and the request/permission registries are leaking unbounded.
        out.push_str("# HELP ocean_gc_failures_total Background registry-GC sweeps that failed.\n");
        out.push_str("# TYPE ocean_gc_failures_total counter\n");
        let _ = writeln!(out, "ocean_gc_failures_total {gc_failures}");

        // SSE consumer-lag counters (OCEAN-372), read from the single source of
        // truth on `AppState`. `sse_lag_events_total` counts `Lagged` occurrences
        // across every SSE connection; `sse_events_dropped_total` sums the events
        // those lags silently dropped. A climbing value means slow consumers are
        // overflowing the broadcast ring and losing events.
        out.push_str(
            "# HELP ocean_sse_lag_events_total SSE subscriber lag occurrences (slow consumers).\n",
        );
        out.push_str("# TYPE ocean_sse_lag_events_total counter\n");
        let _ = writeln!(out, "ocean_sse_lag_events_total {sse_lag_events}");

        out.push_str(
            "# HELP ocean_sse_events_dropped_total Deliverable events dropped by lagging SSE subscribers on unfiltered rails.\n",
        );
        out.push_str("# TYPE ocean_sse_events_dropped_total counter\n");
        let _ = writeln!(out, "ocean_sse_events_dropped_total {sse_events_dropped}");

        out
    }
}

// ── Ocean Rooms §4.1: room + federation metrics ─────────────────────────────

/// The five [`RoomAccessState`] variants, in a frozen order that owns the gauge
/// index and the Prometheus `state=` label. Adding a variant upstream is a
/// compile error here (the array length is checked against the match below),
/// which is the point: a silently unlabelled sixth state would be a room the
/// operator's access-state gauges simply never count.
const ACCESS_STATES: [RoomAccessState; 5] = [
    RoomAccessState::Local,
    RoomAccessState::Connecting,
    RoomAccessState::Live,
    RoomAccessState::Recovering,
    RoomAccessState::Revoked,
];

const fn access_state_index(state: RoomAccessState) -> usize {
    match state {
        RoomAccessState::Local => 0,
        RoomAccessState::Connecting => 1,
        RoomAccessState::Live => 2,
        RoomAccessState::Recovering => 3,
        RoomAccessState::Revoked => 4,
    }
}

const fn access_state_label(state: RoomAccessState) -> &'static str {
    match state {
        RoomAccessState::Local => "local",
        RoomAccessState::Connecting => "connecting",
        RoomAccessState::Live => "live",
        RoomAccessState::Recovering => "recovering",
        RoomAccessState::Revoked => "revoked",
    }
}

/// Why a room invite redemption failed. One variant per
/// `room_federation::IntentError` variant, deliberately re-declared here rather
/// than labelling off the federation enum directly: this file owns what may
/// become a Prometheus label, and a closed local enum is what makes the
/// cardinality of `ocean_room_redemption_failures_total` provably eight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RedemptionFailure {
    Invalid,
    NotFound,
    Conflict,
    Forbidden,
    InviteForbidden,
    Unavailable,
    Protocol,
    Store,
}

impl RedemptionFailure {
    const ALL: [Self; 8] = [
        Self::Invalid,
        Self::NotFound,
        Self::Conflict,
        Self::Forbidden,
        Self::InviteForbidden,
        Self::Unavailable,
        Self::Protocol,
        Self::Store,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Invalid => 0,
            Self::NotFound => 1,
            Self::Conflict => 2,
            Self::Forbidden => 3,
            Self::InviteForbidden => 4,
            Self::Unavailable => 5,
            Self::Protocol => 6,
            Self::Store => 7,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Forbidden => "forbidden",
            Self::InviteForbidden => "invite_forbidden",
            Self::Unavailable => "unavailable",
            Self::Protocol => "protocol",
            Self::Store => "store",
        }
    }
}

/// Why the daemon refused to admit a speaker into a room. Covers both refusal
/// levels: the room-agent admission arms in `room_agent_authority.rs`, every one
/// of which audits `outcome = "refused"` with a reason code, and the member-level
/// post refusals in `persistent_rooms.rs` (`PostRejection`).
///
/// [`Self::classify`] maps an arriving reason-code string onto this closed set
/// with an explicit [`Self::Other`] bucket. That fallback is the whole reason a
/// `&str` is not used as the label directly: two of the upstream reason codes are
/// not literals in the refusal arm at all — one is a binding-status string and
/// one is an `ApiError` code — so labelling off the raw string would put an
/// open-ended vocabulary into a Prometheus label, exactly what
/// `crates/ocean-daemon/AGENTS.md` forbids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AdmissionRefusal {
    BindingMissing,
    OwnerIneligible,
    PackageIdentityMismatch,
    BindingSuspended,
    BindingStale,
    BindingRevoked,
    ActivationPolicyRefused,
    RoomMemoryUnavailable,
    PackageUnresolved,
    ForgedAuthorKind,
    AuthorNotInRoster,
    InvalidThreadParent,
    BodyTooLarge,
    Other,
}

impl AdmissionRefusal {
    const ALL: [Self; 14] = [
        Self::BindingMissing,
        Self::OwnerIneligible,
        Self::PackageIdentityMismatch,
        Self::BindingSuspended,
        Self::BindingStale,
        Self::BindingRevoked,
        Self::ActivationPolicyRefused,
        Self::RoomMemoryUnavailable,
        Self::PackageUnresolved,
        Self::ForgedAuthorKind,
        Self::AuthorNotInRoster,
        Self::InvalidThreadParent,
        Self::BodyTooLarge,
        Self::Other,
    ];

    const fn index(self) -> usize {
        match self {
            Self::BindingMissing => 0,
            Self::OwnerIneligible => 1,
            Self::PackageIdentityMismatch => 2,
            Self::BindingSuspended => 3,
            Self::BindingStale => 4,
            Self::BindingRevoked => 5,
            Self::ActivationPolicyRefused => 6,
            Self::RoomMemoryUnavailable => 7,
            Self::PackageUnresolved => 8,
            Self::ForgedAuthorKind => 9,
            Self::AuthorNotInRoster => 10,
            Self::InvalidThreadParent => 11,
            Self::BodyTooLarge => 12,
            Self::Other => 13,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::BindingMissing => "binding_missing",
            Self::OwnerIneligible => "owner_ineligible",
            Self::PackageIdentityMismatch => "package_identity_mismatch",
            Self::BindingSuspended => "binding_suspended",
            Self::BindingStale => "binding_stale",
            Self::BindingRevoked => "binding_revoked",
            Self::ActivationPolicyRefused => "activation_policy_refused",
            Self::RoomMemoryUnavailable => "room_memory_unavailable",
            Self::PackageUnresolved => "package_unresolved",
            Self::ForgedAuthorKind => "forged_author_kind",
            Self::AuthorNotInRoster => "author_not_in_roster",
            Self::InvalidThreadParent => "invalid_thread_parent",
            Self::BodyTooLarge => "body_too_large",
            Self::Other => "other",
        }
    }

    /// Fold an audited refusal reason code onto the closed label set. The three
    /// binding-status codes arrive as the status string itself
    /// (`binding.status.as_str()`), so they are named apart here rather than
    /// collapsed — "the binding is stale" and "the binding is revoked" are
    /// different operator problems.
    pub(super) fn classify(reason_code: &str) -> Self {
        match reason_code {
            "binding_missing" => Self::BindingMissing,
            "owner_ineligible" => Self::OwnerIneligible,
            "package_identity_mismatch" => Self::PackageIdentityMismatch,
            "suspended" => Self::BindingSuspended,
            "stale" => Self::BindingStale,
            "revoked" => Self::BindingRevoked,
            "activation_policy_refused" => Self::ActivationPolicyRefused,
            "room_memory_unavailable" => Self::RoomMemoryUnavailable,
            "forged_author_kind" => Self::ForgedAuthorKind,
            "author_not_in_roster" => Self::AuthorNotInRoster,
            "invalid_thread_parent" => Self::InvalidThreadParent,
            "body_too_large" => Self::BodyTooLarge,
            _ => Self::Other,
        }
    }
}

/// One room's line on the `/health` rooms card.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct RoomCardEntry {
    pub(super) room_id: String,
    pub(super) access_state: RoomAccessState,
    #[serde(default)]
    pub(super) outbox_pending: u64,
    #[serde(default)]
    pub(super) outbox_failed: u64,
    /// Age of this room's oldest still-unconfirmed outbox row, in seconds, as
    /// measured from when THIS daemon process first saw that row. `None` when
    /// the outbox is empty. See [`RoomMetrics::observe_store_sample`] for why
    /// this is a first-sighting clock and what it does not answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) outbox_oldest_age_seconds: Option<u64>,
    /// Federation SSE lag for this room: the connection's announced snapshot
    /// high-water minus the last sequence this daemon accepted. `None` until an
    /// epoch on this room has reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) federation_lag_events: Option<u64>,
}

/// The `rooms` section of the `GET /health` card: the whole [`RoomMetrics`]
/// registry as JSON, including the per-room list that may never become a
/// Prometheus label.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct RoomMetricsCard {
    /// Whether the most recent scrape actually got the store lock. `false` means
    /// every room-derived number below is the previous sample — see
    /// [`RoomMetrics::note_sample_skipped`].
    pub(super) sampled: bool,
    /// Age of the numbers below, in milliseconds. `None` before the first
    /// successful sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) sample_age_ms: Option<u64>,
    /// Room count by access state — the JSON twin of
    /// `ocean_room_access_state`.
    pub(super) rooms_by_access_state: std::collections::BTreeMap<String, u64>,
    pub(super) outbox_pending: u64,
    pub(super) outbox_failed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) outbox_oldest_age_seconds: Option<u64>,
    pub(super) federation_sse_reconnects_total: u64,
    pub(super) federation_lag_events_max: u64,
    pub(super) redemption_failures_total: u64,
    pub(super) redemption_failures: std::collections::BTreeMap<String, u64>,
    pub(super) admission_refusals_total: u64,
    pub(super) admission_refusals: std::collections::BTreeMap<String, u64>,
    pub(super) store_lock_waits_total: u64,
    pub(super) store_lock_wait_seconds_total: f64,
    /// Per-room detail. This is the surface the fixed-cardinality rule pushes
    /// room identity onto: a room id is unbounded, so it lives here and never in
    /// a Prometheus label.
    pub(super) rooms: Vec<RoomCardEntry>,
}

/// Mutable non-atomic state behind [`RoomMetrics`]: everything that is either
/// per-room (and so cannot be a label) or needs read-modify-write.
#[derive(Default)]
struct RoomMetricsDetail {
    /// Last sampled access state per room, room id -> state.
    access: Vec<(String, RoomAccessState)>,
    /// Last sampled outbox depth per room, room id -> (pending, failed).
    outbox: HashMap<String, (u64, u64)>,
    /// The first-sighting clock. room id -> (client_event_id of the oldest row,
    /// when this process first saw that exact row at the head of the outbox).
    oldest_seen: HashMap<String, (String, Instant)>,
    /// Last reported federation SSE lag per room.
    lag: HashMap<String, u64>,
    /// When the last successful store sample completed.
    sampled_at: Option<Instant>,
    /// Whether the most recent sample ATTEMPT succeeded.
    sampled: bool,
}

/// Room and federation observability counters (Ocean Rooms definition-of-done
/// §4.1), held on `AppState` beside [`TurnMetrics`] and rendered onto both
/// shipped surfaces: Prometheus lines on `GET /metrics`, and the JSON `rooms`
/// card on `GET /health`.
///
/// Exactly six families live here and nothing else:
///
/// 1. rooms by access state (one gauge per [`RoomAccessState`] variant),
/// 2. outbox depth by state (pending, failed) plus oldest-item age,
/// 3. federation SSE reconnects (counter) and lag (gauge),
/// 4. redemption failures by [`RedemptionFailure`],
/// 5. admission refusals by [`AdmissionRefusal`],
/// 6. store lock wait (count and summed wait).
///
/// Families 1 and 2 are SAMPLED from the store at scrape time; the rest are
/// PUSHED from their sites. The atomics are relaxed for the same reason
/// [`TurnMetrics`]'s are: independent counters with no cross-counter invariant.
#[derive(Default)]
pub(super) struct RoomMetrics {
    rooms_by_access_state: [std::sync::atomic::AtomicU64; ACCESS_STATES.len()],
    outbox_pending: std::sync::atomic::AtomicU64,
    outbox_failed: std::sync::atomic::AtomicU64,
    /// Oldest outbox row's age in milliseconds, across every sampled room.
    outbox_oldest_age_ms: std::sync::atomic::AtomicU64,
    federation_reconnects: std::sync::atomic::AtomicU64,
    /// Max per-room lag, which is what a single gauge can honestly say when the
    /// room id may not be a label. Per-room lag is on the JSON card.
    federation_lag_max: std::sync::atomic::AtomicU64,
    redemption_failures: [std::sync::atomic::AtomicU64; RedemptionFailure::ALL.len()],
    admission_refusals: [std::sync::atomic::AtomicU64; AdmissionRefusal::ALL.len()],
    store_lock_waits: std::sync::atomic::AtomicU64,
    store_lock_wait_nanos: std::sync::atomic::AtomicU64,
    detail: std::sync::Mutex<RoomMetricsDetail>,
}

impl RoomMetrics {
    fn detail(&self) -> std::sync::MutexGuard<'_, RoomMetricsDetail> {
        // Same poison recovery the room-store adapters use: a metrics registry
        // must never be the thing that takes a surface down.
        match self.detail.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Fold one store sample into families 1 and 2.
    ///
    /// The outbox AGE is measured in-process from first sighting rather than
    /// from a stored timestamp, because the `outbox` table has no timestamp
    /// column (see `crates/ocean-store/src/lib.rs`'s schema block — the row
    /// carries the producer tuple, content, state, and `position`, and nothing
    /// temporal). The alternative was a nullable `enqueued_at` migration; this
    /// takes the in-process clock instead, and the trade is stated rather than
    /// hidden: what this answers is "how long has THIS daemon process watched
    /// the oldest unconfirmed row sit at the head of the outbox", so a row that
    /// survives a daemon restart is re-aged from zero and the gauge UNDER-reports
    /// after a restart. It never over-reports, and a backlog that is genuinely
    /// stuck climbs again immediately, which is the alerting shape that matters.
    ///
    /// Identity is the `(room, client_event_id)` pair at the head of the outbox,
    /// so confirming the oldest row and starting to age its successor is a fresh
    /// clock rather than an inherited one.
    pub(super) fn observe_store_sample(
        &self,
        projection: &ocean_store::RoomMetricsProjection,
        now: Instant,
    ) {
        use std::sync::atomic::Ordering::Relaxed;

        let mut counts = [0u64; ACCESS_STATES.len()];
        for (_, state) in &projection.access_states {
            counts[access_state_index(*state)] += 1;
        }
        for (slot, value) in self.rooms_by_access_state.iter().zip(counts) {
            slot.store(value, Relaxed);
        }

        let mut detail = self.detail();
        detail.access = projection
            .access_states
            .iter()
            .map(|(key, state)| (key.as_str().to_string(), *state))
            .collect();

        let mut pending_total = 0u64;
        let mut failed_total = 0u64;
        let mut oldest_age_ms = 0u64;
        detail.outbox.clear();
        let mut still_present: HashMap<String, (String, Instant)> = HashMap::new();
        for row in &projection.outbox {
            let room = row.room.as_str().to_string();
            pending_total = pending_total.saturating_add(row.pending);
            failed_total = failed_total.saturating_add(row.failed);
            detail
                .outbox
                .insert(room.clone(), (row.pending, row.failed));
            let Some(oldest) = row.oldest_client_event_id.as_ref() else {
                continue;
            };
            // Keep the existing first-sighting instant only when the head of the
            // outbox is still the SAME row; otherwise this row is new to us and
            // its clock starts now.
            let first_seen = match detail.oldest_seen.get(&room) {
                Some((seen_id, seen_at)) if seen_id == oldest => *seen_at,
                _ => now,
            };
            still_present.insert(room, (oldest.clone(), first_seen));
            let age_ms = u64::try_from(now.saturating_duration_since(first_seen).as_millis())
                .unwrap_or(u64::MAX);
            oldest_age_ms = oldest_age_ms.max(age_ms);
        }
        // Rooms whose outbox drained keep no clock: retaining them would age a
        // row that is no longer there and resurrect it if the id ever recurred.
        detail.oldest_seen = still_present;
        detail.sampled_at = Some(now);
        detail.sampled = true;
        drop(detail);

        self.outbox_pending.store(pending_total, Relaxed);
        self.outbox_failed.store(failed_total, Relaxed);
        self.outbox_oldest_age_ms.store(oldest_age_ms, Relaxed);
    }

    /// Record that a scrape could not take the store lock, so families 1 and 2
    /// are the previous sample. The `/health` liveness probe never blocks on the
    /// room store; it reports staleness instead.
    pub(super) fn note_sample_skipped(&self) {
        self.detail().sampled = false;
    }

    /// One federation SSE reconnect attempt: the room loop finished an epoch and
    /// is going back around to redial.
    pub(super) fn record_federation_reconnect(&self) {
        self.federation_reconnects
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Report this room's federation SSE lag — the epoch's announced snapshot
    /// high-water minus the last sequence accepted on it. Zero means caught up.
    pub(super) fn set_federation_lag(&self, room: &str, lag: u64) {
        let mut detail = self.detail();
        detail.lag.insert(room.to_string(), lag);
        let max = detail.lag.values().copied().max().unwrap_or(0);
        drop(detail);
        self.federation_lag_max
            .store(max, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn record_redemption_failure(&self, reason: RedemptionFailure) {
        self.redemption_failures[reason.index()].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn record_admission_refusal(&self, refusal: AdmissionRefusal) {
        self.admission_refusals[refusal.index()].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// One completed acquisition of the daemon-wide room-store mutex, with how
    /// long the caller waited for it.
    pub(super) fn record_store_lock_wait(&self, waited: Duration) {
        use std::sync::atomic::Ordering::Relaxed;
        self.store_lock_waits.fetch_add(1, Relaxed);
        let nanos = u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX);
        self.store_lock_wait_nanos.fetch_add(nanos, Relaxed);
    }

    /// Project the whole registry as the `GET /health` `rooms` card.
    pub(super) fn card(&self) -> RoomMetricsCard {
        use std::sync::atomic::Ordering::Relaxed;
        let detail = self.detail();
        let now = Instant::now();

        let mut rooms: Vec<RoomCardEntry> = detail
            .access
            .iter()
            .map(|(room_id, state)| {
                let (pending, failed) = detail.outbox.get(room_id).copied().unwrap_or((0, 0));
                let outbox_oldest_age_seconds = detail
                    .oldest_seen
                    .get(room_id)
                    .map(|(_, first_seen)| now.saturating_duration_since(*first_seen).as_secs());
                RoomCardEntry {
                    room_id: room_id.clone(),
                    access_state: *state,
                    outbox_pending: pending,
                    outbox_failed: failed,
                    outbox_oldest_age_seconds,
                    federation_lag_events: detail.lag.get(room_id).copied(),
                }
            })
            .collect();
        rooms.sort_by(|a, b| a.room_id.cmp(&b.room_id));

        let sample_age_ms = detail.sampled_at.map(|at| {
            u64::try_from(now.saturating_duration_since(at).as_millis()).unwrap_or(u64::MAX)
        });
        let sampled = detail.sampled;
        drop(detail);

        let rooms_by_access_state = ACCESS_STATES
            .iter()
            .map(|state| {
                (
                    access_state_label(*state).to_string(),
                    self.rooms_by_access_state[access_state_index(*state)].load(Relaxed),
                )
            })
            .collect();
        let redemption_failures: std::collections::BTreeMap<String, u64> = RedemptionFailure::ALL
            .iter()
            .map(|reason| {
                (
                    reason.label().to_string(),
                    self.redemption_failures[reason.index()].load(Relaxed),
                )
            })
            .collect();
        let admission_refusals: std::collections::BTreeMap<String, u64> = AdmissionRefusal::ALL
            .iter()
            .map(|refusal| {
                (
                    refusal.label().to_string(),
                    self.admission_refusals[refusal.index()].load(Relaxed),
                )
            })
            .collect();
        let outbox_pending = self.outbox_pending.load(Relaxed);
        let outbox_failed = self.outbox_failed.load(Relaxed);

        RoomMetricsCard {
            sampled,
            sample_age_ms,
            rooms_by_access_state,
            outbox_pending,
            outbox_failed,
            outbox_oldest_age_seconds: (outbox_pending + outbox_failed > 0)
                .then(|| self.outbox_oldest_age_ms.load(Relaxed) / 1000),
            federation_sse_reconnects_total: self.federation_reconnects.load(Relaxed),
            federation_lag_events_max: self.federation_lag_max.load(Relaxed),
            redemption_failures_total: redemption_failures.values().sum(),
            redemption_failures,
            admission_refusals_total: admission_refusals.values().sum(),
            admission_refusals,
            store_lock_waits_total: self.store_lock_waits.load(Relaxed),
            store_lock_wait_seconds_total: (self.store_lock_wait_nanos.load(Relaxed) as f64)
                / 1_000_000_000.0,
            rooms,
        }
    }

    /// Render the six families as Prometheus text, appended to the `/metrics`
    /// body. Every label here comes from a closed enum; no room id, member id,
    /// package id, or invite code may become one.
    pub(super) fn render_prometheus(&self) -> String {
        use std::fmt::Write as _;
        use std::sync::atomic::Ordering::Relaxed;

        let mut out = String::with_capacity(2048);

        out.push_str(
            "# HELP ocean_room_access_state Persistent rooms by federation access state.\n",
        );
        out.push_str("# TYPE ocean_room_access_state gauge\n");
        for state in ACCESS_STATES {
            let count = self.rooms_by_access_state[access_state_index(state)].load(Relaxed);
            let _ = writeln!(
                out,
                "ocean_room_access_state{{state=\"{}\"}} {count}",
                access_state_label(state)
            );
        }

        out.push_str(
            "# HELP ocean_room_outbox_depth Unconfirmed locally-authored room events by outbox state.\n",
        );
        out.push_str("# TYPE ocean_room_outbox_depth gauge\n");
        let _ = writeln!(
            out,
            "ocean_room_outbox_depth{{state=\"pending\"}} {}",
            self.outbox_pending.load(Relaxed)
        );
        let _ = writeln!(
            out,
            "ocean_room_outbox_depth{{state=\"failed\"}} {}",
            self.outbox_failed.load(Relaxed)
        );

        out.push_str(
            "# HELP ocean_room_outbox_oldest_age_seconds Age of the oldest unconfirmed outbox row, measured from this process's first sighting of it.\n",
        );
        out.push_str("# TYPE ocean_room_outbox_oldest_age_seconds gauge\n");
        let oldest_seconds = (self.outbox_oldest_age_ms.load(Relaxed) as f64) / 1000.0;
        let _ = writeln!(out, "ocean_room_outbox_oldest_age_seconds {oldest_seconds}");

        out.push_str(
            "# HELP ocean_room_federation_sse_reconnects_total Federation SSE epochs that ended and were redialled.\n",
        );
        out.push_str("# TYPE ocean_room_federation_sse_reconnects_total counter\n");
        let _ = writeln!(
            out,
            "ocean_room_federation_sse_reconnects_total {}",
            self.federation_reconnects.load(Relaxed)
        );

        out.push_str(
            "# HELP ocean_room_federation_lag_events Highest per-room federation SSE lag: announced snapshot high-water minus the last accepted sequence.\n",
        );
        out.push_str("# TYPE ocean_room_federation_lag_events gauge\n");
        let _ = writeln!(
            out,
            "ocean_room_federation_lag_events {}",
            self.federation_lag_max.load(Relaxed)
        );

        out.push_str(
            "# HELP ocean_room_redemption_failures_total Invite redemptions refused, by reason.\n",
        );
        out.push_str("# TYPE ocean_room_redemption_failures_total counter\n");
        for reason in RedemptionFailure::ALL {
            let count = self.redemption_failures[reason.index()].load(Relaxed);
            let _ = writeln!(
                out,
                "ocean_room_redemption_failures_total{{reason=\"{}\"}} {count}",
                reason.label()
            );
        }

        out.push_str(
            "# HELP ocean_room_admission_refusals_total Speakers refused admission to a room, by refusal code.\n",
        );
        out.push_str("# TYPE ocean_room_admission_refusals_total counter\n");
        for refusal in AdmissionRefusal::ALL {
            let count = self.admission_refusals[refusal.index()].load(Relaxed);
            let _ = writeln!(
                out,
                "ocean_room_admission_refusals_total{{code=\"{}\"}} {count}",
                refusal.label()
            );
        }

        out.push_str(
            "# HELP ocean_room_store_lock_waits_total Acquisitions of the daemon-wide room-store mutex through the shared adapters.\n",
        );
        out.push_str("# TYPE ocean_room_store_lock_waits_total counter\n");
        let _ = writeln!(
            out,
            "ocean_room_store_lock_waits_total {}",
            self.store_lock_waits.load(Relaxed)
        );

        out.push_str(
            "# HELP ocean_room_store_lock_wait_seconds_total Summed time spent waiting for the room-store mutex.\n",
        );
        out.push_str("# TYPE ocean_room_store_lock_wait_seconds_total counter\n");
        let wait_seconds = (self.store_lock_wait_nanos.load(Relaxed) as f64) / 1_000_000_000.0;
        let _ = writeln!(
            out,
            "ocean_room_store_lock_wait_seconds_total {wait_seconds}"
        );

        out
    }
}

/// Process-global install point for [`RoomMetrics`].
///
/// The registry's authority is the `AppState` field; this is a second access
/// path for the three push sites that have no `&AppState` in scope and could not
/// get one without threading a parameter through call sites in files other open
/// PRs are editing: the store-lock adapters (`with_rooms_handle` takes only the
/// handle) and the federation supervisor's room loop (`SupervisorInner` is built
/// before and independently of `AppState`).
///
/// It is installed ONLY from the real daemon startup path, never from a test
/// state builder. A test process therefore leaves it unset and those recordings
/// are silent no-ops, which is deliberate: a `OnceLock` shared by many test
/// `AppState`s would attribute one test's lock waits to another test's registry.
/// The families that record through here are pinned by presence on the surface
/// and by direct registry unit tests; the two families whose counts a route test
/// asserts (redemption failures, admission refusals) record through `&AppState`
/// and never through this global.
static PROCESS_ROOM_METRICS: std::sync::OnceLock<Arc<RoomMetrics>> = std::sync::OnceLock::new();

/// Install the daemon's registry as the process-global one. Idempotent; the
/// first install wins and later calls are ignored.
pub(super) fn install_process_room_metrics(metrics: Arc<RoomMetrics>) {
    let _ = PROCESS_ROOM_METRICS.set(metrics);
}

/// Run `f` against the process-global registry when one is installed.
pub(super) fn with_process_room_metrics(f: impl FnOnce(&RoomMetrics)) {
    if let Some(metrics) = PROCESS_ROOM_METRICS.get() {
        f(metrics);
    }
}

/// RAII guard for the [`TurnMetrics::in_flight`] gauge (OCEAN-303). Constructing
/// it bumps the gauge; dropping it (turn finished, cancelled, or panicked) drops
/// the gauge back. Holding it across the whole `runtime.prompt` await is what
/// makes "turns currently running" honest under cancellation — a guillotined
/// turn still decrements on unwind. Saturating-subtracts so the gauge can never
/// underflow into a giant `u64` even if construction/destruction ever skewed.
pub(super) struct InFlightGuard {
    metrics: Arc<TurnMetrics>,
}

impl InFlightGuard {
    pub(super) fn enter(metrics: Arc<TurnMetrics>) -> Self {
        metrics
            .in_flight
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self { metrics }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        // Saturating decrement: never wrap past zero.
        let mut cur = self.metrics.in_flight.load(Relaxed);
        loop {
            let next = cur.saturating_sub(1);
            match self
                .metrics
                .in_flight
                .compare_exchange_weak(cur, next, Relaxed, Relaxed)
            {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }
}

/// RAII guard balancing the advisor provider-call in-flight gauge.
pub(super) struct AdvisorInFlightGuard {
    metrics: Arc<TurnMetrics>,
}

impl AdvisorInFlightGuard {
    pub(super) fn enter(metrics: Arc<TurnMetrics>) -> Self {
        metrics
            .advisor_in_flight
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self { metrics }
    }
}

impl Drop for AdvisorInFlightGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        let mut current = self.metrics.advisor_in_flight.load(Relaxed);
        loop {
            match self.metrics.advisor_in_flight.compare_exchange_weak(
                current,
                current.saturating_sub(1),
                Relaxed,
                Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }
}

/// Pull the integer value of a single non-labelled metric line (e.g.
/// `ocean_turns_in_flight 3`) out of a Prometheus exposition body. Returns
/// `None` if the metric isn't present. Test helper only.
#[cfg(test)]
pub(super) fn metric_value(body: &str, name: &str) -> Option<u64> {
    body.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?;
        // Must be the whole metric name, not a prefix of a longer one, and
        // must be a bare (non-labelled) sample: `name <value>`.
        let rest = rest.strip_prefix(' ')?;
        rest.trim().parse::<u64>().ok()
    })
}

/// Pull a labelled counter sample, e.g.
/// `ocean_turns_total{outcome="ok"} 5` → `5`. Matches the exact
/// `name{labels}` prefix. Test helper only.
#[cfg(test)]
pub(super) fn labelled_value(body: &str, name_with_labels: &str) -> Option<u64> {
    body.lines().find_map(|line| {
        let rest = line.strip_prefix(name_with_labels)?;
        let rest = rest.strip_prefix(' ')?;
        rest.trim().parse::<u64>().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A freshly-defaulted `TurnMetrics` renders a complete, valid Prometheus
    /// exposition: every metric carries its `# HELP`/`# TYPE` header, the
    /// outcome counters and in-flight gauge are present and zero, the histogram
    /// emits one bucket per configured bound plus the `+Inf` bucket, and the
    /// externally-owned `persist_failures` value is surfaced verbatim. This is
    /// the empty-but-honest surface a just-booted daemon exposes.
    #[test]
    fn metrics_render_empty_is_valid_prometheus() {
        let m = TurnMetrics::default();
        let body = m.render_prometheus(0, 0, 0, 0);

        // Every metric has both header lines.
        for stem in [
            "ocean_turns_total",
            "ocean_turns_in_flight",
            "ocean_turn_duration_seconds",
            "ocean_advisor_in_flight",
            "ocean_advisor_outcomes_total",
            "ocean_advisor_duration_seconds",
            "ocean_persist_failures_total",
            "ocean_sse_lag_events_total",
            "ocean_sse_events_dropped_total",
        ] {
            assert!(
                body.contains(&format!("# HELP {stem} ")),
                "missing # HELP for {stem}\n{body}"
            );
            assert!(
                body.contains(&format!("# TYPE {stem} ")),
                "missing # TYPE for {stem}\n{body}"
            );
        }
        assert!(body.contains("# TYPE ocean_turns_in_flight gauge"));
        assert!(body.contains("# TYPE ocean_turn_duration_seconds histogram"));
        assert!(body.contains("# TYPE ocean_turns_total counter"));

        // Zero everywhere on a fresh surface.
        assert_eq!(
            labelled_value(&body, "ocean_turns_total{outcome=\"ok\"}"),
            Some(0)
        );
        assert_eq!(
            labelled_value(&body, "ocean_turns_total{outcome=\"error\"}"),
            Some(0)
        );
        assert_eq!(metric_value(&body, "ocean_turns_in_flight"), Some(0));
        assert_eq!(metric_value(&body, "ocean_advisor_in_flight"), Some(0));
        for outcome in AdvisorOutcome::ALL {
            assert_eq!(
                labelled_value(
                    &body,
                    &format!(
                        "ocean_advisor_outcomes_total{{outcome=\"{}\"}}",
                        outcome.label()
                    )
                ),
                Some(0)
            );
        }
        assert_eq!(
            metric_value(&body, "ocean_advisor_duration_seconds_count"),
            Some(0)
        );
        assert_eq!(
            metric_value(&body, "ocean_turn_duration_seconds_count"),
            Some(0)
        );

        // One explicit bucket per configured bound, plus the implicit +Inf.
        let bucket_lines = body
            .lines()
            .filter(|l| l.starts_with("ocean_turn_duration_seconds_bucket"))
            .count();
        assert_eq!(
            bucket_lines,
            TURN_LATENCY_BUCKETS_MS.len() + 1,
            "expected {} explicit buckets + 1 +Inf bucket",
            TURN_LATENCY_BUCKETS_MS.len()
        );
        assert!(body.contains("ocean_turn_duration_seconds_bucket{le=\"+Inf\"} 0"));
    }

    /// Recording turns folds `wall_ms` into the cumulative histogram and sum, and
    /// counts the outcome. A 120ms turn lands in every bucket whose bound it is
    /// `<=` (i.e. the 250ms bucket and up, NOT the 50/100ms buckets), the `+Inf`
    /// bucket equals the total count, and the seconds-sum reflects the ms total.
    /// This is the core "a turn increments the counter + records its duration"
    /// property, asserted on the exact call the hot path makes.
    #[test]
    fn metrics_record_turn_buckets_and_counts() {
        let m = TurnMetrics::default();
        // Two OK turns (120ms, 300ms) and one error turn (5ms).
        m.record_turn(120, true);
        m.record_turn(300, true);
        m.record_turn(5, false);

        let body = m.render_prometheus(0, 0, 0, 0);

        assert_eq!(
            labelled_value(&body, "ocean_turns_total{outcome=\"ok\"}"),
            Some(2)
        );
        assert_eq!(
            labelled_value(&body, "ocean_turns_total{outcome=\"error\"}"),
            Some(1)
        );
        assert_eq!(
            metric_value(&body, "ocean_turn_duration_seconds_count"),
            Some(3)
        );

        // +Inf bucket == total turns.
        assert_eq!(
            labelled_value(&body, "ocean_turn_duration_seconds_bucket{le=\"+Inf\"}"),
            Some(3)
        );

        // Cumulative buckets. The 5ms error turn is `<= 50ms`, so the 0.05 bucket
        // holds just it (count 1). The 50ms < 120ms turn first appears at the
        // 0.25 bucket. By 0.5s all three (5, 120, 300) are included.
        assert_eq!(
            labelled_value(&body, "ocean_turn_duration_seconds_bucket{le=\"0.05\"}"),
            Some(1),
            "only the 5ms turn is <= 50ms\n{body}"
        );
        assert_eq!(
            labelled_value(&body, "ocean_turn_duration_seconds_bucket{le=\"0.1\"}"),
            Some(1),
            "still only the 5ms turn is <= 100ms\n{body}"
        );
        assert_eq!(
            labelled_value(&body, "ocean_turn_duration_seconds_bucket{le=\"0.25\"}"),
            Some(2),
            "5ms + 120ms turns are <= 250ms\n{body}"
        );
        assert_eq!(
            labelled_value(&body, "ocean_turn_duration_seconds_bucket{le=\"0.5\"}"),
            Some(3),
            "all three turns are <= 500ms\n{body}"
        );

        // Sum is in seconds: (120 + 300 + 5) ms = 0.425 s.
        assert!(
            body.contains("ocean_turn_duration_seconds_sum 0.425"),
            "expected sum 0.425s\n{body}"
        );
    }

    #[test]
    fn metrics_record_advisor_outcomes_and_latency_without_high_cardinality_labels() {
        let m = TurnMetrics::default();
        m.record_advisor(
            AdvisorOutcome::Emitted,
            std::time::Duration::from_millis(40),
        );
        m.record_advisor(
            AdvisorOutcome::ProviderError,
            std::time::Duration::from_millis(120),
        );
        m.record_advisor(AdvisorOutcome::Saturated, std::time::Duration::ZERO);

        let body = m.render_prometheus(0, 0, 0, 0);
        assert_eq!(
            labelled_value(&body, "ocean_advisor_outcomes_total{outcome=\"emitted\"}"),
            Some(1)
        );
        assert_eq!(
            labelled_value(
                &body,
                "ocean_advisor_outcomes_total{outcome=\"provider_error\"}"
            ),
            Some(1)
        );
        assert_eq!(
            labelled_value(&body, "ocean_advisor_outcomes_total{outcome=\"saturated\"}"),
            Some(1)
        );
        assert_eq!(
            metric_value(&body, "ocean_advisor_duration_seconds_count"),
            Some(3)
        );
        assert_eq!(
            labelled_value(&body, "ocean_advisor_duration_seconds_bucket{le=\"0.01\"}"),
            Some(1),
            "only the zero-duration saturation is <= 10ms\n{body}"
        );
        assert_eq!(
            labelled_value(&body, "ocean_advisor_duration_seconds_bucket{le=\"0.05\"}"),
            Some(2),
            "zero + 40ms attempts are <= 50ms\n{body}"
        );
        assert_eq!(
            labelled_value(&body, "ocean_advisor_duration_seconds_bucket{le=\"0.25\"}"),
            Some(3),
            "all attempts are <= 250ms\n{body}"
        );
        assert_eq!(
            labelled_value(&body, "ocean_advisor_duration_seconds_bucket{le=\"+Inf\"}"),
            metric_value(&body, "ocean_advisor_duration_seconds_count"),
            "+Inf must equal the histogram count\n{body}"
        );
        assert!(body.contains("ocean_advisor_duration_seconds_sum 0.16"));
        assert!(!body.contains("turn_id="));
        assert!(!body.contains("session_id="));
        assert!(!body.contains("model="));
    }

    /// The in-flight gauge rises while an [`InFlightGuard`] is alive and falls
    /// back when it drops — the "gauge goes up during a turn and back down after"
    /// property, modelled on the exact RAII bracket the turn hot path uses. Two
    /// concurrent guards stack to 2; dropping both returns to 0.
    #[test]
    fn metrics_in_flight_guard_up_then_down() {
        let metrics = Arc::new(TurnMetrics::default());
        use std::sync::atomic::Ordering::Relaxed;

        assert_eq!(metrics.in_flight.load(Relaxed), 0);
        {
            let _g1 = InFlightGuard::enter(metrics.clone());
            assert_eq!(metrics.in_flight.load(Relaxed), 1, "one turn in flight");
            {
                let _g2 = InFlightGuard::enter(metrics.clone());
                assert_eq!(metrics.in_flight.load(Relaxed), 2, "two turns in flight");
            }
            // Inner guard dropped — back to one.
            assert_eq!(metrics.in_flight.load(Relaxed), 1, "one after inner drop");
        }
        // Both dropped — back to zero, the resting state between turns.
        assert_eq!(metrics.in_flight.load(Relaxed), 0, "zero after all drop");
    }

    // ── Ocean Rooms DoD §4.1 registry ───────────────────────────────────────

    fn projection(
        access: &[(&str, RoomAccessState)],
        outbox: &[(&str, u64, u64, Option<&str>)],
    ) -> ocean_store::RoomMetricsProjection {
        ocean_store::RoomMetricsProjection {
            access_states: access
                .iter()
                .map(|(id, state)| (ocean_core::RoomKey::new(*id), *state))
                .collect(),
            outbox: outbox
                .iter()
                .map(
                    |(id, pending, failed, oldest)| ocean_store::RoomOutboxDepth {
                        room: ocean_core::RoomKey::new(*id),
                        pending: *pending,
                        failed: *failed,
                        oldest_client_event_id: oldest.map(|s| s.to_string()),
                    },
                )
                .collect(),
        }
    }

    /// A sample folds into the access-state gauges and the outbox depths, and a
    /// state with no rooms still renders an explicit zero rather than dropping
    /// out of the exposition.
    #[test]
    fn room_metrics_sample_counts_access_states_and_outbox_depth() {
        let m = RoomMetrics::default();
        m.observe_store_sample(
            &projection(
                &[
                    ("a", RoomAccessState::Live),
                    ("b", RoomAccessState::Live),
                    ("c", RoomAccessState::Recovering),
                    ("d", RoomAccessState::Local),
                ],
                &[("a", 2, 1, Some("ev-1")), ("c", 0, 3, Some("ev-9"))],
            ),
            Instant::now(),
        );

        let body = m.render_prometheus();
        assert_eq!(
            labelled_value(&body, "ocean_room_access_state{state=\"live\"}"),
            Some(2)
        );
        assert_eq!(
            labelled_value(&body, "ocean_room_access_state{state=\"recovering\"}"),
            Some(1)
        );
        assert_eq!(
            labelled_value(&body, "ocean_room_access_state{state=\"local\"}"),
            Some(1)
        );
        assert_eq!(
            labelled_value(&body, "ocean_room_access_state{state=\"connecting\"}"),
            Some(0),
            "an empty state must still be emitted\n{body}"
        );
        assert_eq!(
            labelled_value(&body, "ocean_room_outbox_depth{state=\"pending\"}"),
            Some(2)
        );
        assert_eq!(
            labelled_value(&body, "ocean_room_outbox_depth{state=\"failed\"}"),
            Some(4),
            "failed rows sum across rooms\n{body}"
        );

        // A second sample REPLACES the gauges; it never accumulates.
        m.observe_store_sample(
            &projection(&[("a", RoomAccessState::Live)], &[]),
            Instant::now(),
        );
        let body = m.render_prometheus();
        assert_eq!(
            labelled_value(&body, "ocean_room_access_state{state=\"live\"}"),
            Some(1)
        );
        assert_eq!(
            labelled_value(&body, "ocean_room_access_state{state=\"recovering\"}"),
            Some(0)
        );
        assert_eq!(
            labelled_value(&body, "ocean_room_outbox_depth{state=\"pending\"}"),
            Some(0)
        );
    }

    /// The outbox age is a first-sighting clock keyed on the row's identity: the
    /// same head row keeps ageing across samples, and a NEW head row starts from
    /// zero rather than inheriting its predecessor's age. That second half is the
    /// property that makes the gauge mean "this row is stuck" and not "this room
    /// has been busy for a while".
    #[test]
    fn room_metrics_outbox_age_is_per_row_and_resets_on_a_new_head() {
        let m = RoomMetrics::default();
        let t0 = Instant::now();

        m.observe_store_sample(
            &projection(
                &[("a", RoomAccessState::Live)],
                &[("a", 1, 0, Some("ev-1"))],
            ),
            t0,
        );
        assert_eq!(m.card().outbox_oldest_age_seconds, Some(0));

        // Same head row, 30s later: it has been sitting for 30s.
        m.observe_store_sample(
            &projection(
                &[("a", RoomAccessState::Live)],
                &[("a", 1, 0, Some("ev-1"))],
            ),
            t0 + Duration::from_secs(30),
        );
        assert_eq!(m.card().outbox_oldest_age_seconds, Some(30));

        // `ev-1` confirmed, `ev-2` is now the head: a fresh clock, not 30s.
        m.observe_store_sample(
            &projection(
                &[("a", RoomAccessState::Live)],
                &[("a", 1, 0, Some("ev-2"))],
            ),
            t0 + Duration::from_secs(31),
        );
        assert_eq!(
            m.card().outbox_oldest_age_seconds,
            Some(0),
            "a new head row must not inherit its predecessor's age"
        );

        // Drained: no row, so no age to report.
        m.observe_store_sample(
            &projection(&[("a", RoomAccessState::Live)], &[]),
            t0 + Duration::from_secs(32),
        );
        assert_eq!(m.card().outbox_oldest_age_seconds, None);
    }

    /// The pushed families count under their own closed labels, and an unknown
    /// refusal reason falls into `other` instead of minting a label — the
    /// fixed-cardinality rule this registry exists under.
    #[test]
    fn room_metrics_pushed_families_use_closed_labels() {
        let m = RoomMetrics::default();
        m.record_federation_reconnect();
        m.record_federation_reconnect();
        m.set_federation_lag("room-a", 12);
        m.set_federation_lag("room-b", 5);
        m.record_redemption_failure(RedemptionFailure::Invalid);
        m.record_redemption_failure(RedemptionFailure::Unavailable);
        m.record_admission_refusal(AdmissionRefusal::classify("author_not_in_roster"));
        m.record_admission_refusal(AdmissionRefusal::classify("stale"));
        m.record_admission_refusal(AdmissionRefusal::classify("a_code_nobody_declared"));
        m.record_store_lock_wait(Duration::from_millis(250));
        m.record_store_lock_wait(Duration::from_millis(750));

        let body = m.render_prometheus();
        assert_eq!(
            metric_value(&body, "ocean_room_federation_sse_reconnects_total"),
            Some(2)
        );
        assert_eq!(
            metric_value(&body, "ocean_room_federation_lag_events"),
            Some(12),
            "the single gauge reports the worst room, since a room id cannot be a label\n{body}"
        );
        assert_eq!(
            labelled_value(
                &body,
                "ocean_room_redemption_failures_total{reason=\"invalid\"}"
            ),
            Some(1)
        );
        assert_eq!(
            labelled_value(
                &body,
                "ocean_room_redemption_failures_total{reason=\"unavailable\"}"
            ),
            Some(1)
        );
        assert_eq!(
            labelled_value(
                &body,
                "ocean_room_admission_refusals_total{code=\"author_not_in_roster\"}"
            ),
            Some(1)
        );
        assert_eq!(
            labelled_value(
                &body,
                "ocean_room_admission_refusals_total{code=\"binding_stale\"}"
            ),
            Some(1),
            "a binding-status reason code keeps its own label\n{body}"
        );
        assert_eq!(
            labelled_value(&body, "ocean_room_admission_refusals_total{code=\"other\"}"),
            Some(1),
            "an undeclared reason code must fall into `other`, never mint a label\n{body}"
        );
        assert!(
            !body.contains("a_code_nobody_declared") && !body.contains("room-a"),
            "no unbounded string may reach a label\n{body}"
        );
        assert_eq!(
            metric_value(&body, "ocean_room_store_lock_waits_total"),
            Some(2)
        );
        assert!(
            body.contains("ocean_room_store_lock_wait_seconds_total 1"),
            "250ms + 750ms is one second of summed wait\n{body}"
        );

        // Lag falls back as a room catches up; the gauge is not a high-water mark.
        m.set_federation_lag("room-a", 0);
        let body = m.render_prometheus();
        assert_eq!(
            metric_value(&body, "ocean_room_federation_lag_events"),
            Some(5)
        );
    }

    /// A skipped sample marks the card stale rather than zeroing it: `/health`
    /// must never turn store-lock contention into "there are no rooms".
    #[test]
    fn room_metrics_skipped_sample_keeps_the_previous_numbers() {
        let m = RoomMetrics::default();
        m.observe_store_sample(
            &projection(
                &[("a", RoomAccessState::Live)],
                &[("a", 3, 0, Some("ev-1"))],
            ),
            Instant::now(),
        );
        assert!(m.card().sampled);

        m.note_sample_skipped();
        let card = m.card();
        assert!(!card.sampled, "a skipped sample must say so");
        assert_eq!(
            card.rooms_by_access_state["live"], 1,
            "and keep the numbers"
        );
        assert_eq!(card.outbox_pending, 3);
        assert_eq!(card.rooms.len(), 1);
    }

    /// The in-flight gauge never underflows: a stray decrement at zero saturates
    /// at zero rather than wrapping to `u64::MAX`. Guards a future refactor that
    /// might double-drop or skew enter/exit from corrupting the gauge.
    #[test]
    fn metrics_in_flight_never_underflows() {
        use std::sync::atomic::Ordering::Relaxed;
        let metrics = Arc::new(TurnMetrics::default());
        // Force a drop with no matching enter by faking the guard's Drop body via
        // a real guard whose enter we then "undo" twice: simplest is to drop a
        // guard, then drop another guard constructed when the count is already 0.
        let g = InFlightGuard::enter(metrics.clone());
        assert_eq!(metrics.in_flight.load(Relaxed), 1);
        drop(g); // -> 0
        assert_eq!(metrics.in_flight.load(Relaxed), 0);
        // Directly exercise the saturating decrement at zero.
        let g2 = InFlightGuard::enter(metrics.clone()); // -> 1
        metrics.in_flight.store(0, Relaxed); // skew: pretend it was already 0
        drop(g2); // saturating_sub(1) on 0 stays 0, no wrap
        assert_eq!(
            metrics.in_flight.load(Relaxed),
            0,
            "decrement at zero must saturate, never wrap to u64::MAX"
        );
    }
}
