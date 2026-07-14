//! In-process turn metrics and Prometheus text rendering.

use std::sync::Arc;

/// Upper bounds (inclusive, milliseconds) of the turn-latency histogram buckets.
/// A turn that took `wall_ms` increments every bucket whose bound it is `<=`,
/// matching Prometheus cumulative-histogram semantics (`le` = "less than or
/// equal"). The implicit `+Inf` bucket is the total turn count and is emitted
/// separately. Chosen to span sub-second turns through multi-minute agent loops:
/// 50ms / 100ms / 250ms / 500ms / 1s / 2.5s / 5s / 10s / 30s / 60s / 120s.
const TURN_LATENCY_BUCKETS_MS: [u64; 11] = [
    50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 120_000,
];

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

        let mut out = String::with_capacity(1024);

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
