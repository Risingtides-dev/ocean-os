# Ocean Extension Stage A5 — Integrated Gate Evidence

**Date:** 2026-09-26

**Status:** A5 is implemented on `feat/extension-stage-a5` and is **pending
independent review**. This document is evidence for review. It does **not**
accept Stage A. Stage A acceptance still needs the operator rulings in §5 and a
separate operator acceptance, as §20 step 15 of the manifest requires.

**Governing contract:**
[`2026-07-27-ocean-extension-stage-a-implementation-manifest.md`](2026-07-27-ocean-extension-stage-a-implementation-manifest.md)
(§18 A5, §19, §20). Parent:
[`2026-07-14-ocean-extensions-architecture-and-migration-manifest.md`](2026-07-14-ocean-extensions-architecture-and-migration-manifest.md)
(Phases 2–3).

**Base:** `origin/main` at `37f0200b` (A4 delta-review follow-ups, PR #504).
**Head:** the head commit of the A5 pull request, recorded in its description.

## 1. What A5 adds

A5 adds tests only. It adds no route, schema, persisted field, wire frame,
lifecycle kind, or runtime behavior. The only non-test source edits are:

- `#[cfg(test)]` module declarations for the new test files;
- `pub(super)` on the A3b route-test helpers, so the gate can reuse them;
- one `doctor` route on the test-only A3b router.

New test files:

| File | What it holds |
| --- | --- |
| `crates/ocean-daemon/src/extension_registry/mutation/stage_a_gate.rs` | The §20 integrated gate. It uses the real §15 routes, the A3a writer, the supervisor, the lifecycle dispatcher, the project-create route, and ordinary turns through `agent_turn` (the keyless `fake-ok` model, and `fake-tool` for the permission and tool path). They run over one real no-op native service. The service speaks the strict stdio protocol, persists its cursor under `data/`, and records every frame it receives. The gate decodes every recorded host frame with the strict SDK decoder and requires it to re-encode byte-exactly. It also has the structural boundary tests. |
| `crates/ocean-daemon/src/extension_service/tests/a5_gaps.rs` | Unit and process closures for partial §19.1–§19.2 rows. |
| `crates/ocean-daemon/src/extension_registry/tests/a5_gaps.rs` | Reader closures for §19.4. |
| `crates/ocean-daemon/src/extension_registry/transaction/tests/a5_gaps.rs` | Writer closures for §19.4. |
| `crates/ocean-daemon/src/main.rs` (one test) | `a5_permission_policy_allow_session_and_deny_resolve_once_without_reasons` |
| `crates/ocean-cli/src/extension.rs` (one test) | `a5_mutations_are_sent_exactly_once_whatever_the_answer` |

Run them with:

```text
cargo test -p ocean-daemon stage_a_gate
cargo test -p ocean-daemon a5_
cargo test -p ocean-cli a5_
cargo test -p ocean-daemon a5_stable_reset -- --ignored   # ~5.5 min, real time
```

The §20 no-op fixture is a POSIX shell script, as in every earlier slice's
process tests. §20 specifies "a minimal Rust service"; this is a deviation that
needs operator acceptance (§5 R-5). A Rust fixture binary would need a new
workspace member or build step, and the manifest requires an amendment for a
new crate (§6), so A5 did not add one. The script has everything else §20 asks
of the fixture: handshake, a cursor persisted under `data/`, pong, ACK, a
cooperative grandchild, and a secret echo on stderr in the dedicated redaction
case. Because the script only records lines, the gate proves host-side
conformance itself: every recorded host→child frame is decoded with
`ocean_agent_sdk::extension_lifecycle::decode_frame` into its declared v1 type
(`HostHello`, `Ready`, `LifecycleEvent`, `Lag`, `Reset`, `Ping`, `Shutdown`) and
must re-encode to the identical bytes. Child→host strictness stays proven by the
SDK and transport tests (§4.1).

## 2. §20 gate steps

`G` means `extension_registry::mutation::stage_a_gate`.

| Step | Proof | Status |
| --- | --- | --- |
| 1 local install offline; no marker/process | `G::stage_a_gate_local_noop_package_end_to_end` (committed, untrusted, no `state/`, no start, no canary) and the same test's counting resolver and Git canary, which are never touched | proven |
| 2 inspect/doctor/list/status execute nothing | same test (`package_code_executed: false`, empty status, zero starts, canaries) | proven |
| 3 trust preview notice; exact grant; not enabled, not running | same test (notice equals `NATIVE_AUTHORITY_NOTICE`, exact `service-grants.json` row at revision 2, `enabled:false`, zero starts) | proven |
| 4 enable: identity, roots, minimal env, readiness, one group | same test: `host_hello` identity equals digest, revision, epoch, and floor. `HOME`/`XDG_*`/`TMPDIR`/`PWD`/cwd/argv[0] are checked against the assigned roots by inode. The host passes descriptor-derived paths (`/.vol/<dev>/<ino>` on macOS, `/proc/self/fd/<n>` on Linux) that only the child can resolve, so the child records the inode each one names and the gate compares it. The environment names are exactly the §11.3 set, a planted daemon variable is absent, and leader and grandchild share one PGID. | proven |
| 5 new/resumed sessions with permission and tools: scoped metadata, no payloads, client/SSE compatibility | same test: kinds are exactly `session_started, turn_started, turn_finished, turn_started, turn_finished` (the resumed turn has no `session_started`); the prompt and cwd sentinels are absent from every frame; the ACK and ordinary client event types equal a no-extension baseline. Permission and tool: `G::stage_a_gate_live_permission_and_tool_facts_are_metadata_only`. A `fake-tool` turn asks for `write`, the real daemon policy suspends it on a waiter, and the operator allows it through that waiter. The service receives exactly `session_started, turn_started, permission_requested, permission_resolved(allowed), tool_started(write), tool_finished(write, success), turn_finished`, with host-UUID `permission_id`/`tool_call_id`, and no path, content, prompt, or runtime tool-call id. A denied second turn yields `permission_resolved(denied)` and no tool facts. | proven. The ordinary client/SSE comparison uses the no-tool turn only. |
| 6 lag, then retained replay and reset; turns stay responsive | Lag under real backpressure: `G::stage_a_gate_stalled_service_never_delays_ordinary_turns`. It fires 160 concurrent ordinary turns (at least 480 facts, far beyond the 256-frame queue and the 64 KiB pipe) at a service that never reads stdin again. It proves `lag_count > 0`, that every turn finishes within its bound, and that the blocked write fails the connection (`stopping`, `protocol_violation`) no earlier than 2 s after the burst began and within 3.5 s of its end, followed by bounded cleanup and reap. Replay: `G::stage_a_gate_crash_resume_backoff_circuit_and_explicit_retry` (a same-epoch resume after a process failure replays exactly the unprocessed facts, with no reset). Reset: step 7 and step 9. | proven. The stalled child never reads, so the *content* of a delivered `lag` frame is proven at unit level (`SVC5::a5_live_queue_bounds_count_and_bytes_with_computed_replay_availability`), not on a live wire. |
| 7 disable → events → stale re-enable; project widening | `G::stage_a_gate_local_noop_package_end_to_end` (new epoch, floor ≥ interval high-water mark, `reset: activation_changed`, no interval sequence or session delivered). Widening, live: `G::stage_a_gate_project_scope_widening_never_replays_interval_facts`. Two projects are created through the real route and the service is enabled for A: A's facts arrive carrying A's `project_id`, and B's facts never arrive. Widening to A+B mints a new epoch with a floor at or above every B interval fact, the stale cursor gets `reset: activation_changed`, no B interval fact is ever delivered, and B's facts after the widening are delivered. | proven |
| 8 crash loop: numeric backoff, circuit, generation-safe cleanup | `G::stage_a_gate_crash_resume_backoff_circuit_and_explicit_retry`: measured gaps of 1 s and 2 s between crash starts (tolerance −60/+500 ms, so a 1 s→2 s change fails, M11b), `circuit_open` with `restart_count 4`, every leader and grandchild dead, no timer re-close within 5 s (past the 4 s step an un-opened circuit would take), and disable → enable retries with fresh history | proven |
| 9 daemon restart | `G::stage_a_gate_local_noop_package_end_to_end`: `shutdown reason daemon_stopping`, leader and grandchild gone, a new dispatcher boot id through the production `start_extension_host`, a stale cursor gets `reset: boot_changed`, every event is on the new boot, `data/` is retained, and exactly one service runs | proven |
| 10 disable one project scope and the global scope | global: steps 7, 11 and 12 of the local gate (filter/epoch, shutdown, child and grandchild reaped, `data/` retained) | global: proven. Project-scope disable of a shared service: **blocked**, §5 R-2 |
| 11 active remove/update refused; exact update untrusted, not started | same test (`extension_active` ×2 while live; the update digest is untrusted, `enable` → `trust_required`, zero new starts) | proven |
| 12 remove preserving state, identical reinstall/retrust, purge | same test (`data/` kept, `cache/` gone, no temp root, grants emptied, identical digest reinstalls untrusted and reuses `data/`; the purge removes `state/<id>` and revokes a *live* trust/grant/enable row) plus `transaction::tests::a3a_retention_matrix_update_rollback_remove_reinstall_and_purge` | proven. See §4.4 for the per-cell table. |
| 13 exact public HTTPS Git root commit, pinned, for install and update; adjacent sources refused | `G::stage_a_gate_pinned_git_noop_installs_untrusted_and_activates_only_after_trust`. Install: same digest as the local tree, untrusted until trust, runs only after enable, reaped on disable. Update, live: after disable, an exact second commit installs a new untrusted digest with Git provenance, `enable` → `trust_required`, and no start. Branch, tag, `HEAD`, short, uppercase, userinfo, `http`, `ssh`, and `subdir` are all refused before any acquisition. Rollback: `GT::a4_git_update_rollback_and_failed_update_keep_trust_cleared`. Socket-level pinning: `transaction::git::tests::a4_pinned_connection_reaches_only_the_checked_address` (+ IPv6, SNI). Credential-requiring: `a4_credential_challenges_are_never_answered`. Public smoke: `a4_public_commit_smoke` (`#[ignore]`, recorded 2026-09-25 in the A4 note). | proven. The gate's fetch uses the A4 test-only `file://` remote behind the full resolution and public-address check; the pin itself is proven by the A4 loopback tests. |
| 14 extension absent: no observable change | same test (after purge, a turn's ACK and client event types equal the pre-install baseline) | proven |
| 15 full gates, independent review, operator acceptance | §8 gates; review pending; operator acceptance pending | **pending** |

## 3. Status legend

- **proven**: an earlier-slice test that asserts the claim (test body checked).
- **A5**: proven by a test this slice adds.
- **partial**: the named part is proven and the rest is stated.
- **blocked**: needs an operator ruling (§5).
- **open**: not proven, with the reason (§6).

## 4. §19 acceptance matrix

Abbreviations: `LC` = `extension_lifecycle::tests`, `SVC` =
`extension_service::tests`, `SVC5` = `extension_service::tests::a5_gaps`,
`ER` = `extension_registry::tests`, `ER5` = `extension_registry::tests::a5_gaps`,
`TX` = `extension_registry::transaction::tests`, `TX5` =
`extension_registry::transaction::tests::a5_gaps`, `MU` =
`extension_registry::mutation::tests`, `G` =
`extension_registry::mutation::stage_a_gate`, `GT` =
`extension_registry::transaction::git::tests`, `MAIN` = `ocean-daemon` `tests`,
`SDK` = `ocean-agent-sdk` `tests/extension_lifecycle.rs`, `CLI` =
`ocean-cli` `extension::tests`.

### 4.1 Protocol and lifecycle (§19.1)

| Row / claim | Proof | Status |
| --- | --- | --- |
| Golden encode/decode | `SDK::golden_handshake_frames_round_trip_byte_exact`, `SDK::all_ten_closed_metadata_variants_have_byte_exact_golden_fixtures` | proven |
| Unknown-field rejection | `SDK::unknown_fields_and_unsupported_versions_are_rejected`, `SDK::metadata_kind_mismatch_unknown_fields_and_non_v4_event_ids_are_rejected`, `SVC::frame_read_failures_separate_exit_io_and_protocol_causes` | proven |
| Version rejection, before readiness | `SDK::unknown_fields_and_unsupported_versions_are_rejected`; production runner: `SVC5::a5_production_handshake_rejects_duplicate_expanded_versioned_and_late_hello` (version 2 never reaches `ready`) | A5 |
| Frame rejection (non-object, trailing, duplicate key, invalid UTF-8) | `SDK::framing_rejects_non_object_trailing_duplicate_line_and_invalid_utf8`, `SDK::duplicate_keys_are_rejected_in_every_nested_object_boundary`, `SVC::transport_rejects_oversize_duplicate_unknown_and_resume_frames` | proven |
| 65,536 accepted / 65,537 rejected | `SDK::valid_encode_and_decode_accept_65536_and_reject_65537` (both sides); transport reject side in `SVC::transport_rejects_…` | proven (the codec is exact both ways; the transport reader is tested on the reject side) |
| Handshake identity cannot be overridden | `SDK::subscription_must_be_duplicate_free_subset_and_cannot_override_identity`, `SVC::transport_rejects_…`; live: `G::stage_a_gate_local_…` (`host_hello.identity` equals the activation) | proven |
| Subscription is an exact subset | `SDK::subscription_must_be_duplicate_free_subset_…`; production runner: `SVC5::a5_production_handshake_…` (an expanded subscription never reaches `ready`) | A5 |
| Missing / late / duplicate hello fails | missing: `SVC::supervisor_immediately_retries_and_retains_exceptional_startup_cleanup`; late and duplicate: `SVC5::a5_production_handshake_…` | A5 |
| Nine produced kinds have source-table tests | `LC::nine_produced_kinds_map_and_reserved_session_stopped_never_emits`. Call sites: `MAIN::explicit_session_create_publishes_only_the_successful_authoritative_fact`, `MAIN::extension_lifecycle_follows_admission_and_terminal_order_without_changing_ack`, `MAIN::permission_policy_publishes_waiting_then_exactly_one_resolution_without_payloads`, `G::stage_a_gate_boot_and_stop_facts_have_one_ordered_producer_each` (`daemon_started` before the host, `daemon_stopping` after serve and before drain), and `G::stage_a_gate_live_permission_and_tool_facts_are_metadata_only` (permission and tool facts from the real policy and runtime bridge) | A5 |
| All ten schema variants have fixtures; `session_stopped` non-emission | `SDK::all_ten_closed_metadata_variants_…`, `LC::nine_produced_kinds_…`, `LC::graceful_stop_is_exactly_once_and_rejects_every_later_producer` | proven |
| Explicit-create producer | `MAIN::explicit_session_create_publishes_only_the_successful_authoritative_fact` | proven |
| Ordinary `SessionCreated` follows admission | `MAIN::extension_lifecycle_follows_admission_…`, `MAIN::longhouse_turn_preparation_call_sites_and_order_are_exact` | proven |
| Resumed sessions omit `session_started` | `LC::ordinary_and_explicit_session_admission_helpers_preserve_truthful_order`; live through `agent_turn`: `G::stage_a_gate_local_…` | A5 |
| Rejected admission emits nothing | `MAIN::agent_turn_busy_session_conflicts_before_turn_started_or_registration`, `MAIN::explicit_project_workspace_and_resumed_session_mismatches_publish_nothing` | proven |
| Permission resolves once: allow, cancellation, waiter closure | `MAIN::permission_policy_publishes_…`, `MAIN::permission_waiter_cancel_race_is_deterministically_lifecycle_cancelled`, `MAIN::permission_waiter_closure_is_separately_classified_as_cancelled`, `LC::permission_waiting_resolution_pair_is_exactly_once_for_all_terminal_inputs` | proven |
| Permission resolves once: allow-session, deny; reasons and args absent | `MAIN::a5_permission_policy_allow_session_and_deny_resolve_once_without_reasons` (real policy, exactly `[requested, resolved]`, fixed outcome, the reason and argument sentinels absent from the encoded wire) | A5 |
| Runtime permission denial fabricates no tool execution | `LC::compatibility_permission_denial_never_fabricates_execution`; live through the real bridge: `G::stage_a_gate_live_permission_and_tool_facts_are_metadata_only` (a denied turn yields no tool facts) | A5 |
| Tool cancellation from `details.cancelled`; unmatched End is a fixed diagnostic | `LC::tool_uuid_correlation_cancellation_and_unmatched_end_are_honest` | partial: adapter level; see §6 O-1 |
| One turn outcome for normal, cancel race, failure, panic/orphan | `LC::all_terminal_sources_and_cancellation_races_finalize_exactly_once`, `LC::normal_orphan_and_panic_settlement_owners_share_one_authority`, `LC::terminal_authority_compare_exchange_allows_one_competing_owner`, `MAIN::finalizer_*`, `MAIN::captured_terminal_guard_survives_abort_before_spawned_future_first_poll`; live: `G::stage_a_gate_local_…` delivers exactly two `turn_finished` for two turns. That is an observation of the normal path, not an exactly-once guard; the guard is the terminal-authority test set above. | proven at the authority; live normal path observed |
| Existing SDK client status unchanged | `MAIN::cancelled_request_state_overrides_res_derived_failed_terminal_frame`, `MAIN::terminate_orphaned_turn_closes_the_cancelled_agent_rail`; live: `G::stage_a_gate_local_…` (ACK and client event types equal the no-extension baseline) | A5 |
| `session_stopped` never inferred or emitted | `LC::nine_produced_kinds_…`, `LC::graceful_stop_…`; live: the gate subscribes to four kinds and receives only those | proven |
| Concurrent-session ordering by sequence and project scope | `LC::concurrent_session_order_is_global_sequence_and_per_scope_filterable`, `LC::concurrent_publishers_broadcast_in_the_same_order_as_allocated_sequences` | proven |
| A disabled scope observes nothing | `LC::project_scope_is_captured_at_publication_and_delivery_does_not_widen`; live global disable: `G::stage_a_gate_local_…` (interval facts never delivered); live out-of-scope project: `G::stage_a_gate_project_scope_…` (project B's facts are never delivered while only A is enabled) | A5. Disabling one project of a shared service is blocked (§5 R-2). |
| Sentinel payloads cannot serialize | `LC::metadata_only_wire_strips_every_forbidden_source_sentinel`, `SDK::lifecycle_wire_contains_no_forbidden_payload_field_names`, `LC::tool_uuid_…` (args, results, details), `MAIN::a5_permission_policy_…` (reason, args); live: `G::stage_a_gate_local_…` (prompt, cwd path), `G::stage_a_gate_bound_secret_never_leaves_the_spawn_environment` (secret value on the event wire) | proven. Header, env and canvas are structural: the SDK types have no field for them. |
| Observatory unchanged; the observer cannot mutate, cancel, or publish | `G::stage_a_gate_lifecycle_and_observatory_share_no_authority` (neither side references the other's modules, buses, store, or auth); `SVC5::a5_observer_frames_cannot_publish_or_command` (event-, cancel- and subscribe-shaped child frames are protocol violations and the dispatcher sequence is unchanged) | A5 |

### 4.2 Queue, replay and failure isolation (§19.2)

| Row / claim | Proof | Status |
| --- | --- | --- |
| Boot ring count (2,048) and byte (8 MiB) eviction | `LC::boot_ring_evicts_oldest_at_count_bound_and_keeps_sequence`, `LC::boot_ring_evicts_oldest_at_byte_bound` | proven |
| Oversized event rejected before publication | `LC::invalid_or_oversized_daemon_versions_leave_all_bookkeeping_unchanged` | partial: see §6 O-2 |
| Per-service queue: 256 frames and 1 MiB | `SVC5::a5_live_queue_bounds_count_and_bytes_with_computed_replay_availability` (exactly 256 queued; the rest coalesce into one `lag`; a byte-full queue lags the next frame; a reservation landing exactly on 1 MiB is accepted and one byte more is refused without reserving) | A5 |
| Slow reader gets a coalesced `lag`; `replay_available` is computed | `SVC::slow_reader_hits_bounded_data_queue_and_records_coalesced_lag`; `SVC5::a5_live_queue_bounds_…` (true while retained and eligible; false once evicted or out of scope) | A5 |
| Control priority `shutdown > reset > lag > ping`, bounded coalescing | `SVC::prioritized_control_lane_coalesces_without_an_unbounded_control_queue`, `SVC5::a5_control_lane_orders_lag_before_ping_and_coalesces_exactly` | A5 |
| Never delays a turn, permission, or session | turns and sessions: `G::stage_a_gate_stalled_service_never_delays_ordinary_turns` (160 concurrent new-session turns all finish, within a bound, while the service's queue overflows and its stdin is blocked); `LC::concurrent_publishers_…` (publication is synchronous and non-blocking) | A5 for turns and sessions. A permission under live backpressure is not separately driven; the permission producer uses the same non-blocking `publish` (§6 O-9). |
| Resume needs a matching boot, epoch and eligible cursor; each reset reason exact | `SVC::activation_epoch_replay_rejects_widening_old_boot_and_ineligible_cursors`, `SVC5::a5_replay_plan_names_every_reset_reason_exactly` (`activation_changed` for another epoch or below the floor, `retention_exceeded`, `invalid_cursor`, and the exact `oldest/latest_available`; `boot_changed` is the `SVC` test's); live: `G::stage_a_gate_local_…` (`activation_changed`, `boot_changed`), `G::stage_a_gate_project_scope_…` (`activation_changed` on widening), `G::stage_a_gate_crash_…` (a valid same-epoch cursor replays with no reset) | A5 |
| Disable → events → re-enable, and widening, cannot replay the interval | global: `G::stage_a_gate_local_…`; widening: `SVC::activation_epoch_replay_…` (unit) and `G::stage_a_gate_project_scope_widening_never_replays_interval_facts` (live) | A5 |
| Abrupt exit, invalid stdout, stderr flood, startup timeout, ping timeout, crash loop, open circuit are fail-soft | `SVC::abrupt_leader_exit_cleans_surviving_grandchild_before_reap`, `SVC::post_ready_clean_eof_racing_leader_poll_is_always_unexpected_exit`, `SVC::malformed_post_ready_frame_is_a_protocol_violation`, `SVC::supervisor_production_path_cleans_protocol_circuit_and_shutdown`, `SVC::stderr_binary_newline_free_and_rate_flood_stay_bounded_and_redacted`, `SVC::startup_timeout_cleans_process_group_and_preserves_reason`, `SVC::three_missed_pongs_trigger_ping_timeout_and_full_cleanup`, `SVC::on_failure_crash_loop_opens_circuit_after_exact_threshold`, `SVC::scope_only_epoch_change_preserves_open_circuit_and_does_not_respawn`; live: `G::stage_a_gate_crash_…` | proven |
| Oversize stdout | `SVC::transport_rejects_oversize_duplicate_unknown_and_resume_frames` (reader) | proven at the reader |
| Blocked stdin fails at 2 s | `SVC::blocked_stdin_fails_at_the_two_second_connection_deadline` (unit); live: `G::stage_a_gate_stalled_…` (the connection fails as `protocol_violation` no earlier than 2 s after the burst began and within 3.5 s of its end; raising the deadline to 6 s (M25) or cutting it to 200 ms (M26) fails the test) | A5 |
| Backoff sequence | `SVC::restart_backoff_schedule_and_circuit_threshold_match_the_ratified_policy` (constants); measured: `G::stage_a_gate_crash_…` (1 s, 2 s) | A5 |
| Rolling 60 s window | `SVC5::a5_rolling_window_prunes_failures_older_than_sixty_seconds` (four failures 30 s old plus one open the circuit; 61 s old do not) | A5 |
| Stable 5-minute reset | `SVC5::a5_stable_reset_after_five_healthy_minutes_restores_the_first_backoff` (`#[ignore]`, real time, ~5.5 min; run and recorded in §8) | A5 (ignored test, recorded run) |
| Explicit retries close the circuit | disable → enable: `G::stage_a_gate_crash_…` (after `circuit_open`); new digest and daemon restart: history is keyed to the managed generation, so a new digest and a new supervisor start from `RestartHistory::default()` (`SVC::activation_descriptor_ignores_the_global_revision_but_not_the_package_generation`, `MU::disable_then_enable_before_a_pass_clears_restart_history`) | disable→enable A5; digest/restart partial (§6 O-3) |
| Paused Tokio time where possible | ping/pong tests use paused time; backoff and window use seeded history and real time (a real child under paused time is flaky; see the `SVC` comment above `three_missed_pongs…`) | as stated |
| Crash produces no false `daemon_stopping`; restart uses a new boot id | `G::stage_a_gate_boot_and_stop_facts_have_one_ordered_producer_each` (the one producer is after serve returns; a dispatcher that is never gracefully stopped holds no `daemon_stopping`); `G::stage_a_gate_local_…` (new boot id, `boot_changed`) | A5 |

### 4.3 Process and security (§19.3)

| Row / claim | Proof | Status |
| --- | --- | --- |
| `env_clear` baseline exact (names and values) | names: `SVC::strict_hello_ready_transport_and_minimal_environment_succeed`; names and values plus a planted daemon variable: `G::stage_a_gate_local_…` | A5 |
| Cwd, executable and digest bind to one artifact | `SVC::verified_executable_generation_survives_concurrent_path_replacement`, `SVC::unlinked_verified_executable_fails_closed_without_selecting_replacement` (macOS), `ER::anchored_package_handle_survives_digest_path_replacement`; cwd and argv[0] against the digest-named store root: `G::stage_a_gate_local_…` | A5 for cwd. A cwd-under-replacement race is not separately tested (§6 O-4). |
| Only confirmed ordinary/secret names injected | `ER::sole_reader_derives_exact_noop_activation_without_resolving_or_executing`, `SVC::environment_resolution_is_explicit_and_secret_values_are_debug_redacted`, `SVC::spawned_secret_target_is_exact_and_sentinel_never_reaches_status_surfaces`; over HTTP: `G::stage_a_gate_bound_secret_…` (the target equals the value; the source name is absent in the child) | A5 |
| Missing, duplicate, unsupported, reserved, stale bindings fail before spawn | `SVC::missing_reserved_and_unsupported_bindings_fail_before_spawn`, `ER::hand_authored_service_grant_fixtures_cover_valid_invalid_and_bounds`, `TX::a3a_trust_requests_are_validated_before_any_preview`, `TX::a3a_enable_rejects_untrusted_incompatible_declared_and_unbound_packages`, `extension_service_unsupported::tests::real_startup_rejects_malformed_nonexistent_service_and_unauthorized_binding`; `SVC5::a5_missing_secret_source_fails_before_spawn` (run level: no child, `secret_missing`, no temp root); `ER5::a5_service_grant_reader_rejects_order_trust_service_and_binding_authority` (duplicate reference, `OCEAN_EXTENSION_*` and `TMPDIR` targets, ungranted reference) | A5 |
| Secret sentinel absent from state, journal, argv, HTTP, status, events | `G::stage_a_gate_bound_secret_…` (every file under `extensions/` except the child's own `data/`, every mutation and read response, the status cache debug form, retained lifecycle events, the child's `ps` argv, and the delivered frames; the child's stderr echo of the secret is counted, `stderr_redactions >= 1`, on the retained unhealthy status row after the leader is killed, because disable prunes the row); journals are removed at commit, so the scan sees what survives; the journal and staged files are built from the same name-only types (`SecretBinding` holds `target_env` and `reference`), and no crash-point journal is scanned for a value (§6 O-5) | A5 |
| … and from diagnostics, logs, panic/debug, crash output | stderr → counters only: `SVC::runtime_status_has_no_argv_environment_secret_or_diagnostic_text_fields`, `SVC::live_a2b_process_replays_acks_redacts_stderr_and_cleans_on_cancel`; debug: `SVC::environment_resolution_is_explicit_…` | partial: see §6 O-5 |
| Stderr cap, rate, redaction under newline-free and binary input | `SVC::stderr_binary_newline_free_and_rate_flood_stay_bounded_and_redacted` | proven (counters) |
| Child and grandchild gone after abrupt exit, restart/circuit, disable, health failure, daemon shutdown, Git timeout | abrupt: `SVC::abrupt_leader_exit_…`; restart/circuit and disable: `G::stage_a_gate_crash_…` (every recorded leader and grandchild); disable and daemon shutdown: `G::stage_a_gate_local_…`; health: `SVC::supervisor_health_failure_cleans_through_managed_owner` (leader, same group-kill path); Git: `GT::a4_timeout_kills_the_whole_git_process_group`, `GT::a4_a_leader_exit_never_leaves_a_surviving_descendant` | A5, except the health-failure grandchild (same `killpg` path; §6 O-6) |
| PGID reuse never signals an unrelated process | `SVC::reaped_real_leader_and_simulated_reused_pgid_issue_no_signal_syscall`, `SVC::exceptional_signal_error_preserves_authority_for_bounded_retry` | proven (simulated reuse, per the in-code note) |
| Windows: `unsupported_platform`, no child | `extension_service_unsupported::tests::real_startup_projects_only_common_validated_state_and_never_starts_a_child` (runs on macOS/Linux as a source simulation); the `tests/windows-portability` harness cross-compiles only | **blocked**: §5 R-3 |
| State/cache/temp roots reject symlink/path replacement; temp cleaned; data persists or purges as asked | `SVC::assigned_roots_reject_symlink_replacement`, `SVC::assigned_root_descriptors_survive_leaf_and_state_replacement_through_spawn`, `SVC::temp_cleanup_is_descriptor_relative_and_refuses_a_replacement_generation`, `SVC::temp_cleanup_removes_read_only_subtrees`, `TX::a3a_retention_matrix_…`; data across disable, restart, remove and reinstall: `G::stage_a_gate_local_…` | proven |

### 4.4 Registry and package management (§19.4)

| Row / claim | Proof | Status |
| --- | --- | --- |
| A0 suite green | `ER::absent_extensions_directory_is_empty_and_read_only`, `ER::partial_malformed_and_revision_mismatched_state_fail_closed`, `ER::busy_state_lock_is_bounded`, `ER::anchored_state_root_survives_path_replacement_without_following_it`, `ER::open_file_at_type_checks_special_entries_before_opening`, `ER::anchored_package_handle_survives_digest_path_replacement`, `ER::symlinked_state_file_is_rejected`, `ER::fifo_state_and_package_entries_fail_without_blocking`, `ER::directory_enumeration_stops_at_remaining_entry_budget`, `ER::registered_project_override_selects_enablement_but_not_trust`, `ER::overbroad_grant_never_widens_manifest_capabilities`, `ER::inspect_and_doctor_http_envelopes_are_exercised_end_to_end`, `TX::a3a_local_acquisition_rejects_unsafe_trees_and_leaves_nothing_behind` (depth, hardlink, FIFO at acquisition), `GT::a4_package_limits_apply_to_the_listing_before_extraction` | proven. A5 adds a hardlink inside a payload (`ER5::a5_hardlinked_payload_file_invalidates_the_artifact`) and the 1 MiB state/manifest caps (`ER5::a5_oversized_state_file_and_manifest_fail_closed`). |
| `service-grants.json` strictness and A0 absence upgrade | `ER::service_grant_order_duplicates_unknown_fields_and_revision_fail_closed`, `ER::serialized_coherent_reader_accepts_1024_service_grants_and_rejects_1025`, `ER::hand_authored_…`, `ER::a0_absence_four_file_mixed_generation_and_downgrade_are_exact`, `ER::native_acknowledgement_is_exact_and_absence_executes_nothing`, `TX::a3a_first_publication_marker_makes_later_companion_absence_fail_closed`; `ER5::a5_service_grant_reader_…` (distinct descending rows, no trust, unknown service, binding authority, duplicate reference, reserved targets) | A5 |
| Expected revision and exclusive lock on every mutation; no mixed generation | `TX::a3a_expected_revision_and_exclusive_lock_serialize_writers`, `TX::a3a_readers_never_observe_a_mixed_generation_during_commits`, `MU::busy_registry_is_retryable_for_readers_and_writers`; `TX5::a5_enable_update_and_remove_check_expected_revision_before_any_write` | A5 |
| Four acquisitions without the lock; reads during a held fetch | `TX::a3a_acquisition_runs_outside_the_state_lock_with_four_permits`, `TX::a3a_recovery_sweep_and_acquisitions_share_one_registry_gate` | partial: §6 O-7 |
| Crash injection at every journal step | `TX::a3a_crash_at_every_journal_step_…` (11 points × six operations), `TX::a3a_bootstrap_publication_is_one_atomic_rename`, `MU::startup_recovery_rolls_journals_forward_and_back_before_readers` | proven |
| Install ≠ trust ≠ enable; changed digest loses trust; project enable cannot widen | `TX::a3a_install_trust_enable_are_distinct_and_execute_no_package_code`, `ER::changed_payload_digest_invalidates_artifact_and_trust`, `TX::a3a_enable_rejects_…`, `ER::registered_project_override_selects_enablement_but_not_trust`; live: `G::stage_a_gate_local_…`, `G::stage_a_gate_pinned_git_…` | proven |
| Grant preview mutates nothing; stale/wrong confirmation fails; exact applies; diffs stable | `TX::a3a_grant_diff_is_stable_and_confirmation_binds_revision_and_notice`, `TX::a3a_install_trust_enable_…`, `MU::committed_202_is_authoritative_and_a_retry_conflicts_instead_of_recommitting` | partial: no-mutation, stale/wrong, and exact apply are proven, as is stability for widening from nothing. Stability for narrowing, binding-only, native-ack-only, and widening an existing grant has no repeat-preview test (§6 O-8). |
| Enable rejects untrusted, incompatible, network, filesystem, unsupported platform | `TX::a3a_enable_rejects_…`; `TX5::a5_filesystem_declarations_are_refused_like_network`; unsupported platform: §5 R-3 | A5 (filesystem); platform blocked |
| Pre-commit vs committed-202 responses; retry by returned revision | `MU::envelope_states_cover_every_reconciliation_outcome`, `MU::committed_202_…`, `MU::committed_recovery_error_is_500_with_committed_revision_and_fixed_text`, `MU::a_dead_mutation_task_answers_outcome_unknown`, `CLI::exit_codes_follow_the_committed_contract`; `CLI::a5_mutations_are_sent_exactly_once_whatever_the_answer` (a stub daemon counts exactly one request per mutation for 200, 202, 409 retryable, 500 recovery, and 500 `outcome_unknown`, with exit codes 0/3/1/4/5) | A5 |
| Active update/remove refuse; disable removes scope and reaps | `TX::a3a_active_update_and_remove_refuse_without_mutation`, `MU::http_lifecycle_reconciles_supervisor_and_disable_reaps_before_200`; live: `G::stage_a_gate_local_…` | proven |
| Every §12.4 retention cell | see the cell table below | as marked |
| Local install is offline; canaries untouched | by construction (`acquire_local` holds no resolver); `G::stage_a_gate_local_…` registers a counting resolver and a canary `git` for the config and proves neither is touched by local install or update; canaries: every `MU`/`TX`/`G` test | A5 |
| Git refusals, capability floor, pinning, public smoke | `GT::a4_git_source_grammar_is_exact`, `GT::a4_invalid_grammar_refuses_before_permit_quarantine_or_process`, `GT::a4_redirects_are_never_followed`, `GT::a4_credential_challenges_are_never_answered`, `GT::a4_non_public_or_failed_resolution_refuses_the_whole_acquisition`, `GT::a4_every_git_process_gets_a_stripped_environment_and_one_checked_pin`, `GT::a4_symlinks_submodules_filters_and_unsafe_trees_are_refused`, `GT::a4_timeout_kills_the_whole_git_process_group`, `GT::a4_object_ceiling_applies_to_a_real_fetch`, `GT::a4_package_limits_apply_to_the_listing_before_extraction`, `GT::a4_revision_must_name_the_exact_fetched_commit`, `GT::a4_git_capability_fails_closed_without_pinning_support`, `GT::a4_pinned_connection_reaches_only_the_checked_address`, `GT::a4_pinned_https_keeps_hostname_sni_and_tls`, `GT::a4_public_commit_smoke` (ignored; recorded); `MU::strict_bodies_and_git_sources_fail_before_any_acquisition`; `G::stage_a_gate_pinned_git_…` | proven. "Any indication the pin was not honored" has no runtime signal (A4 boundary 1). |

§12.4 retention, cell by cell. `RM` = `TX::a3a_retention_matrix_…`, `CR` =
`TX::a3a_crash_at_every_journal_step_…`, `GL` = `G::stage_a_gate_local_…`.

| Row | Install | Update | Remove (keep) | Remove (purge) | Reinstall identical |
| --- | --- | --- | --- | --- | --- |
| install row + source | RM, GT | RM, GT | RM | MU, GL | RM, GL |
| trust rows | RM | RM, CR | GL (update had cleared them) | GL (live trust revoked) | RM, GL (untrusted) |
| service-grant rows | GT, GL | RM, CR | GL | GL (live row revoked) | GL |
| enablement rows | RM | RM | RM | GL (live row revoked) | RM, GL |
| payloads | CR | RM, CR | RM | RM, GL | RM, GL (same digest) |
| `data/` | GL (not created by install) | GL (kept across update) | RM, CR, GL | RM, GL | RM, GL (reused) |
| `cache/` | GL (created at activation, not install) | GL (kept across update) | RM, GL | RM, GL | GL (recreated at activation) |
| `tmp/` | GL (none until activation) | GL (none after reap) | TX `a3a_active_update_and_remove_refuse_without_mutation` (require-empty), RM, GL | RM, GL | GL (new per connection) |
| journal/audit | TX residue checks | CR | CR | RM | GL (no trust or audit carried) |

## 5. Blocked on operator rulings

These rows cannot be proven without a decision. A5 does not invent behavior to
cover any of them.

| Id | Ruling needed | Rows it blocks | Current behavior (A3b/A4 reading) |
| --- | --- | --- | --- |
| R-1 | **Mutation credential class** (A3b boundary 1). §15 names no credential. | §19.4 HTTP/CLI rows as authorization policy; §20 steps 1–13 as an accepted operator surface | Every mutation requires the local `X-Ocean-Operator` principal; reads are credential-free. `MU::every_mutation_is_operator_authenticated_and_reads_stay_credential_free` proves the reading, not the ruling. |
| R-2 | **§17 project-scope disable of a shared service** (A3b boundary 2). §17 says a project disable that leaves another effective scope must not stop the shared service; the accepted A2b supervisor restarts it under a new epoch. | §20 step 10 (project-scope disable); the live form of §19.1 "one project cannot observe a disabled scope" when another scope stays effective | Every scope change, including the widening the gate proves live, is an activation change: new epoch, restart, preserved restart history. |
| R-3 | **Windows R5** "may inspect/manage packages". The writer is Unix-only and the supervisor is macOS/Linux-only. | §19.3 "Windows status is `unsupported_platform` and no child starts" at runtime; §19.4 "enable rejects unsupported platform" | Non-Unix mutation routes answer 409 `unsupported_platform`; runtime proof exists only as a macOS/Linux source simulation and a cross-compile harness. A Windows CI runtime lane would need its own decision. |
| R-4 | **Local-source path collisions** (A4 boundary 7). Git acquisition refuses case/normalization-colliding paths; local acquisition does not. | §19.4 "Git and local sources of the same tree produce the same digest" for colliding trees | Local sources keep the tree as it exists on one filesystem. |
| R-5 | **The §20 fixture is a shell script, not "a minimal Rust service".** A Rust binary needs a new crate, and §6 requires an amendment for one. | §20's fixture wording, for every gate step | The gate uses a POSIX `sh` fixture. Host→child conformance is proven by strict SDK decoding plus byte-exact re-encoding of every recorded host frame; child→host strictness is proven by the SDK and transport tests. The operator either accepts the script or authorizes a fixture crate. |

## 6. Open (not proven), with reasons

| Id | Claim | Why it is open | Smallest next step |
| --- | --- | --- | --- |
| O-1 | `details.cancelled` extraction at the real runtime-bridge call site (tool start/finish and the denial arm are now live-proven) | The live `fake-tool` path never cancels a running tool; only the adapter maps `cancelled` | A bridge test that cancels a running tool |
| O-2 | Oversized-event rejection branch (`OversizedEvent` diagnostic) | Every lifecycle source field is capped upstream (tool names at 256 bytes, versions by SemVer), so no current source reaches the encoder limit; the branch is defensive | Document as unreachable, or inject a synthetic frame in a lifecycle unit test |
| O-3 | Circuit retry by new digest and by daemon restart, end to end | Holds by construction (fresh history per generation or supervisor); only disable→enable is driven live | Extend the gate's crash test with update→retrust, and with a supervisor restart |
| O-4 | Cwd survives package-path replacement mid-spawn | `fchdir` on the anchored descriptor; the executable race is tested, the cwd race is not | Copy the executable race test and have the child record `pwd -P` |
| O-5 | Secret absent from `tracing` logs, panic payloads, crash output, and a journal captured at each crash point | No log-capture harness exists in the daemon tests; journals are scanned only after commit removes them | A `tracing` test writer around the secret gate; a secret-bound trust in the crash-point loop with a byte scan of each journal |
| O-6 | Grandchild probe after a health (ping) failure | The cleanup path is the same `killpg` group path proven for disable/circuit/shutdown, but the ping-timeout test has no grandchild | Add a grandchild to `three_missed_pongs…` |
| O-7 | All four acquisitions held while the lock and reads are rechecked; `list` read over HTTP during the held fetch | The A3a test checks lock freedom with one live acquisition and reads inspect/doctor over HTTP | Reorder the A3a test as the §19.4 report suggests |
| O-8 | Repeat-preview stability for narrowing, binding-only, native-ack-only, and widening-an-existing-grant diffs | Confirmation binds the canonical diff and notice; repeat-preview equality is tested only for widening from nothing | Repeat-preview pairs in `TX::a3a_grant_diff…` |
| O-9 | A permission waiter under live service backpressure | The stalled gate drives no-tool turns; the permission producer is the same non-blocking `publish` | Run the `fake-tool` permission round-trip during the stalled burst |

## 7. Mutation checks

Each new guard was broken in production code, and the named test was run
against the broken build. Every listed mutation failed its test. The source was
restored after each run.

| # | Mutation (production code) | Test that fails |
| --- | --- | --- |
| M1 | `replay_plan` ignores the activation epoch | `SVC5::a5_replay_plan_names_every_reset_reason_exactly` (the live gate still passes, because its stale cursor is also below the new floor) |
| M2 | `retention_exceeded` / `invalid_cursor` swapped | `SVC5::a5_replay_plan_…` |
| M3 | `ping` popped before `lag` | `SVC5::a5_control_lane_…` |
| M4 | the oldest ping nonce kept | `SVC5::a5_control_lane_…` |
| M5 | failure window never prunes | `SVC5::a5_rolling_window_…` |
| M6 | production handshake skips the subscription-subset check | `SVC5::a5_production_handshake_…` |
| M7 | a missing secret classified as a missing ordinary variable | `SVC5::a5_missing_secret_source_fails_before_spawn` |
| M8 | a new epoch's replay floor set to 0 | `G::stage_a_gate_local_…` |
| M9 | `env_clear` removed from service spawn | `G::stage_a_gate_local_…` |
| M10 | the leader signaled instead of the process group | `G::stage_a_gate_local_…`, `G::stage_a_gate_crash_…` |
| M11 | the third backoff step changed from 1 s to 3 s | `G::stage_a_gate_crash_…` |
| M11b | the third backoff step changed from 1 s to 2 s | `G::stage_a_gate_crash_…` (upper slack +500 ms) |
| M12 | service-grant order checks only equality | `ER5::a5_service_grant_reader_…` |
| M13 | secret-binding authority unchecked | `ER5::a5_service_grant_reader_…` |
| M14 | a hardlinked payload file admitted | `ER5::a5_hardlinked_payload_file_…` |
| M15 | the manifest size cap raised fourfold | `ER5::a5_oversized_state_file_and_manifest_fail_closed` |
| M16 | the expected revision checked only as "not ahead" | `TX5::a5_enable_update_and_remove_check_expected_revision_…` |
| M17 | a `filesystem` grant admitted at trust | `TX5::a5_filesystem_declarations_are_refused_like_network` |
| M18 | allow-session published as `denied` | `MAIN::a5_permission_policy_…` |
| M19 | `stop_publication` moved before the host starts | `G::stage_a_gate_boot_and_stop_facts_…` |
| M20 | the per-service queue raised to 300 frames | `SVC5::a5_live_queue_…` |
| M21 | the per-service byte bound doubled | `SVC5::a5_live_queue_…` |
| M22 | `replay_available` ignores the activation scope | `SVC5::a5_live_queue_…` |
| M23 | the stable reset never applies | `SVC5::a5_stable_reset_…` (ignored test, run for the check) |
| M24 | the CLI sends a mutation twice | `CLI::a5_mutations_are_sent_exactly_once_…` |
| M25 | the stdin write deadline raised from 2 s to 6 s | `G::stage_a_gate_stalled_…` |
| M26 | the stdin write deadline cut from 2 s to 200 ms | `G::stage_a_gate_stalled_…` |
| M27 | frames refused by a full queue dropped without `lag` | `G::stage_a_gate_stalled_…` |
| M8b | a new epoch's replay floor set to 0 | `G::stage_a_gate_project_scope_widening_…` |
| M28 | an allowed permission published as `denied` | `G::stage_a_gate_live_permission_and_tool_facts_…` |
| M30 | the 1 MiB byte bound made exclusive | `SVC5::a5_live_queue_…` |
| M31 | stderr redactions not counted | `G::stage_a_gate_bound_secret_…` |

Not mutation-checked, with the reason:
- The secret-sentinel gate is a negative scan. Its positive control is that the child sees exactly the bound value.
- Strict host-frame decoding in the gate: the host serializes the same SDK types the decoder reads, so no one-line production change produces a non-canonical frame; the check guards against future transport changes.
- The Observatory-isolation test is a structural source check.
- The per-service count bound is enforced by a channel constructed with `OUTBOUND_MAX_MESSAGES` at one production site. M20 checks the constant. The construction site was checked by inspection.

## 8. Gates

Recorded on the A5 head after the Knox review fixes (see the pull request for the exact commit). Each mutation run restored the source file by `move`, which kept an older mtime and could leave a stale mutated build, so every source file was touched and rebuilt before these results were recorded:

Local results on macOS arm64 (Darwin 25.5), rustc stable, host Git 2.50.1, 2026-09-26:

| Gate | Result |
| --- | --- |
| `cargo check --workspace` | pass |
| `cargo test -p ocean-daemon` | 1094 passed, 0 failed, 4 ignored |
| `cargo test -p ocean-daemon stage_a_gate` | 9 passed, three consecutive runs with no flake |
| `cargo test -p ocean-daemon a5_` | 13 passed, 1 ignored, three consecutive runs with no flake |
| `cargo test -p ocean-daemon a5_stable_reset -- --ignored` | passed (310.5 s) on the first A5 head. Neither the test nor any production code has changed since. |
| `cargo test -p ocean-cli` | 21 passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass |
| `cargo clippy -p ocean-daemon --all-targets --features legacy-chromium -- -D warnings` | pass |
| `cargo clippy -p ocean-daemon --all-targets --all-features -- -D warnings` | pass |
| `cargo fmt --all -- --check` | pass |
| `cargo xtask docs-check` | pass (30 packages, 167 active Markdown files, 209 local links) |
| `node scripts/check-ledger.mjs` | pass (655 entries) |
| `cargo deny check` | advisories, bans, licenses, and sources ok |
| `git diff --check` | clean |

Not run locally, because these are CI lanes: `cargo xtask ci --compatibility`,
`cargo +1.88.0 xtask ci --msrv`, and the Ubuntu matrix. The gate and every
`a5_` process test are compiled for macOS and Linux (`cfg(any(target_os =
"macos", target_os = "linux"))`), so the Ubuntu `cargo test` lane runs them
there, where the child's assigned paths are `/proc/self/fd/<n>` spellings. The
gate compares inodes the child resolved, so both spellings are covered. The tests need
Rust 1.82 or later (`Option::is_none_or`), which is within the 1.88 MSRV.
