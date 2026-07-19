# ocean-walker

Standalone native filesystem traversal and scan caching for Ocean OS. M1 is a
library checkpoint only: it is not wired to runtime `grep`/`glob`, the daemon,
agent, TUI, harness profiles, or any production capability.

## API and behavior

- `WalkRequest` builds owned or streaming walks with hidden-file, gitignore,
  `.git`, `node_modules`, depth, filesystem, symlink, metadata, ordering,
  filtering, ranking, limit, cache, and empty-result recheck policy.
- `CollectedEntry::native_relative_path` retains exact native path identity and
  is the only relative field used to construct operational paths.
  `display_path` is a normalized, potentially lossy UTF-8 projection used only
  for display, filtering, and deterministic ordering.
- `walk_entries` and `collect_entries` expose lower-level visitor and owned
  collection paths with caller-supplied heartbeat interruption.
- `for_each_file_candidate_parallel`, `execute_candidates`, and related helpers
  use one centralized Rayon pool while preserving the documented unordered
  parallel and deterministic serial contracts.
- `invalidate_path`, `invalidate_path_string`, and `invalidate_all` invalidate
  shared owned-scan cache state.

The cache defaults to a 1,000 ms TTL, a 200 ms empty-result recheck, and a
strict maximum of 16 entries. Cache roots and invalidation targets share one
absolute lexical namespace; normalization does not canonicalize or follow
links. Age starts only after a successful scan, fresh-versus-hit provenance is
explicit even for sub-millisecond hits, failed scans are never cached, and
invalidation/scan generations prevent invalidated or older in-flight scans from
publishing stale results. These process-global values are read once from
`FS_SCAN_CACHE_TTL_MS`, `FS_SCAN_EMPTY_RECHECK_MS`, and
`FS_SCAN_CACHE_MAX_ENTRIES`; a maximum of `0` disables publication.
`OCEAN_WALK_WORKERS` is also read once: `0` auto-detects available parallelism,
`1` forces serial work, values are capped at 32, and the default is 4 workers.
Dedicated-pool construction failure explicitly falls back to one serial worker.

## Security and adoption boundary

`FollowLinks` and `same_file_system` are snapshot traversal-selection policies.
They are not a sandbox and do not confine a walk to its initial root. M1 does
not authorize traversal of untrusted or adversarial roots and remains unwired
from live `grep`, `glob`, or search.

Before any live runtime adoption, the consuming design must pass a separate
security gate that provides point-of-use descriptor/handle-relative confinement
for adversarial roots, rename plus symlink/reparse-point swaps, cached
candidates, and every supported operating system. Descriptor-relative traversal
itself is intentionally outside M1. Heartbeats are checked at request entry
(including empty and cache-hit paths) and periodically during traversal;
cancellation can still be deferred while a platform API returns one giant
native directory batch.

## Validation

```bash
cargo test -p ocean-walker
cargo clippy -p ocean-walker --all-targets -- -D warnings
```

`tests/perf.rs` retains the donor's ignored deterministic timing harness. It is
excluded from ordinary tests because it creates more than 15,000 files; run it
explicitly in release mode with ignored tests, output enabled, and one test
thread when comparative timing evidence is needed.

## Provenance

The implementation and donor tests are adapted from `can1357/oh-my-pi`
`crates/pi-walker` at exact commit
`03c48d073bd4849726cc14750b5aecfa310bdf26`. Ocean adaptations rename crate,
thread, environment, documentation, and temporary-path vocabulary and narrowly
translate Rust 2024-only syntax to Rust 2021 for the Rust 1.88 MSRV. See
`LICENSE` and `NOTICE`.
