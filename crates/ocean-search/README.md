# ocean-search

Standalone Rust 2021 typed byte and filesystem search for Ocean OS (MSRV 1.88).
It is a workspace member outside `default-members` and has no runtime, daemon,
agent, TUI, capability, profile, tool-schema, or session wiring.

## Contract

- `search_bytes` / `search_bytes_with_heartbeat` search in-memory bytes.
- `search_path` / `search_path_with_heartbeat` search one file or directory over
  `ocean-walker` with cache disabled, `FollowLinks::Never`, `.git` skipped,
  explicit hidden/gitignore policy, and donor-compatible `node_modules` pruning.
- `PatternMode::{Regex,Literal,RegexOrLiteral}` uses Rust's linear-time regex
  implementation only. Multiline is explicit. Fallback provenance is returned.
- `OutputMode` selects real typed `Content`, `Count`, or `FilesWithMatches`
  output; count/file modes do not emit content sentinels. Match/count/offset units
  are grep-searcher matching records (normally matching lines), not individual
  regex occurrences; one multiline record may span several lines.
- Every filesystem result carries an absolute native path, exact native relative
  path, and separate lossy normalized display path. Only the display projection
  participates in glob filtering and display; operational identity never does.
- Strict OR globs normalize basename-only patterns to recursive `**/pattern`.
  `NativeTypeFilter` compares owned native extensions/basenames without path
  reconstruction from lossy text.
- Every candidate is freshly opened once. Unix leaf opens use no-follow and
  nonblocking flags followed by opened-handle regular-file validation. Windows
  uses the reparse-point open flag and rejects reparse metadata on a best-effort
  basis. Fixed chunks are read through that one handle.
- NUL classification is centralized before all modes. Files above the validated
  per-file byte cap (4 MiB default) are skipped and counted; no prefix is
  searched. Returned text alone may use lossy UTF-8 and is byte-bounded with
  character-boundary-safe truncation.
- Candidate windows complete through ocean-walker's centralized parallel helper
  before ordinal commit. Each file receives a deterministic share of the staged
  text budget; offset and output limits apply in commit order, and saturation
  stops later windows while allowing only the admitted window to overscan.
- The supplied root itself is explicit: hidden/gitignore/`.git`/`node_modules`
  traversal policies apply below directory roots, not to an explicit file root.

All allocation-related request controls are finite and cross-validated against
hard engine ceilings; `max_global_items` bounds accepted candidates, and the
result-text budget also bounds text staged by a path window. Heartbeats run
before validation and around traversal, open, read chunks, search callbacks,
commit, and successful return. A blocking native filesystem syscall, candidate
sort, regex compilation, or one matcher invocation already in progress cannot
be preempted; cancellation remains cooperative. Non-multiline matching retains
the donor's LF line-terminator behavior; returned CRLF records trim the ending,
but regex `$` is not promoted to a separate CRLF-aware dialect.

## Security boundary

This is a trusted-root, path-based engine, **not root confinement**. Live runtime
adoption still requires descriptor/handle-relative authorization against root
and intermediate-component rename, symlink/reparse, and cached-candidate swaps
on every supported OS. Do not infer such authorization from walker follow-link
or same-filesystem selection.

## Donor provenance

Mechanisms and focused tests were adapted from MIT-licensed Oh My Pi commit
`03c48d073bd4849726cc14750b5aecfa310bdf26`. This crate does not claim full donor
parity: PCRE2, regex repair, automatic multiline, oversized-prefix search,
N-API/TypeScript, runtime policy, and timing/performance claims are excluded.
See `NOTICE` and `LICENSE`.

## Validation

```bash
cargo test -p ocean-search
cargo clippy -p ocean-search --all-targets -- -D warnings
cargo +1.88.0 test -p ocean-search
```
