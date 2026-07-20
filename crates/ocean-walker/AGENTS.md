# ocean-walker — Standalone Filesystem Walker

## Purpose

Own the M1 standalone native filesystem traversal, filtering, parallel
candidate delivery, and bounded TTL scan-cache library.

## Ownership

- **Scope:** `crates/ocean-walker/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Does not own:** typed content search (owned standalone by `ocean-search`),
  runtime `grep`/`glob`, capability or harness policy, N-API, shell execution,
  or production wiring

## Local Contracts

- Preserve traversal, symlink, ignore/filter/pruning, directory-error,
  heartbeat/cancellation, ordering, and parallel/serial semantics.
- `WalkRequest` is the high-level builder; `walk_entries` and
  `collect_entries` are the low-level visitor and owned-entry entry points.
- Shared owned-scan cache keys include an absolute lexically normalized root
  and traversal options, default to a 1,000 ms TTL, recheck empty hits after
  200 ms, and have a strict 16-entry bound. Fresh/hit provenance is explicit;
  age begins after successful scans; generation checks linearize invalidation
  and concurrent publication; failed scans are never cached. Invalidation is
  lexical and deliberately does not canonicalize or follow links.
- Cache configuration is process-global and read once from
  `FS_SCAN_CACHE_TTL_MS`, `FS_SCAN_EMPTY_RECHECK_MS`, and
  `FS_SCAN_CACHE_MAX_ENTRIES`. The centralized pool similarly reads
  `OCEAN_WALK_WORKERS` once; `0` auto-detects, `1` forces serial work, values
  are capped at 32, and pool construction failure reports/uses serial behavior.
- Owned entries retain exact native relative `PathBuf` identity for filesystem
  operations. Lossy normalized UTF-8 fields are display/filter projections
  only and must never be used to reconstruct paths.
- `FollowLinks` and `same_file_system` select a snapshot traversal; they are not
  sandbox or root-confinement controls. M1 does not authorize untrusted roots.
  Live adoption requires a separate point-of-use descriptor/handle-relative
  confinement gate covering swaps, cached candidates, and supported OSes.
- Keep Rust 2021, Rust 1.88 compatibility, and warnings denied.
- Keep the crate standalone and outside `default-members`; `ocean-search` may
  depend on this crate, but no live runtime, grep, glob, agent, daemon, or TUI
  dependency/wiring belongs in M1.
- Preserve pinned Oh My Pi attribution in `LICENSE` and `NOTICE`.

## Work Guidance

Port behavior narrowly rather than redesigning it. The ignored donor timing
harness is retained in `tests/perf.rs`, but ordinary validation does not run it:
it creates more than 15,000 files and is intended for explicit release-style,
single-threaded measurement only.

## Verification

- `cargo test -p ocean-walker`
- `cargo clippy -p ocean-walker --all-targets -- -D warnings`
- `cargo +1.88.0 test -p ocean-walker` when the pinned toolchain exists
- Performance harness (optional):
  `cargo test -p ocean-walker --test perf --release -- --ignored --nocapture --test-threads=1`

## Child devlog Index

No child boundaries defined.
