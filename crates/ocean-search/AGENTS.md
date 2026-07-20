# ocean-search — Standalone Typed Search

## Purpose

Own the M1 standalone bounded typed in-memory and trusted-root filesystem search
engine over `ocean-walker`.

## Ownership

- **Scope:** `crates/ocean-search/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Does not own:** runtime grep/glob adoption, daemon/agent/TUI behavior,
  capabilities/profiles/tool schemas, N-API, PCRE2, or path authorization

## Local Contracts

- Preserve plain typed Rust `Content`, `Count`, and `FilesWithMatches` outputs,
  typed regex/glob/request/interruption errors, provenance, and wide summaries.
  Match/count/offset units are grep-searcher matching records, not regex occurrences.
- Rust linear-time regex only. Literal fallback is explicit and attributable;
  multiline is opt-in. Do not add repair heuristics.
- Preserve exact native absolute/relative path identity. Lossy normalized paths
  are only display/glob projections and must never be operationally rebuilt.
- Search candidates with cache disabled and `FollowLinks::Never`; fresh-open
  each candidate once, validate the opened handle, and read fixed chunks up to
  the hard validated cap plus one classifier byte. Oversized files are skipped.
- Centralize NUL classification before all modes and bound result allocation
  before worker staging as well as through validated finite maxima and ordered commit.
- Treat the supplied root as explicit: descendant hidden/ignore/pruning policy
  must not silently reject a direct file root. Zero output limits admit no candidates.
- Keep deterministic path-window completion through ocean-walker's centralized
  helper. Heartbeat before validation and successful return, around I/O, during
  callbacks, and during commit. Interruption never returns partial success.
- This trusted-root path engine is not confinement. Runtime adoption requires a
  separately accepted descriptor/handle-relative authorization design covering
  every supported OS and all root/intermediate/candidate swaps.
- Keep Rust 2021 / Rust 1.88, standalone, outside `default-members`, and retain
  pinned donor attribution in `LICENSE`, `NOTICE`, tests, and README.

## Work Guidance

Port mechanisms narrowly from pinned Oh My Pi commit
`03c48d073bd4849726cc14750b5aecfa310bdf26`; do not port N-API/TypeScript,
PCRE2, regex repair, auto-multiline, oversized-prefix search, or runtime policy.

## Verification

- `cargo test -p ocean-search`
- `cargo clippy -p ocean-search --all-targets -- -D warnings`
- `cargo +1.88.0 test -p ocean-search`
- `cargo check --workspace --tests`

## Child devlog Index

No child boundaries defined.
