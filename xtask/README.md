# xtask — Ocean OS dev-task runner

A plain workspace binary following the [cargo-xtask](https://github.com/matklad/cargo-xtask)
pattern: repo automation written in Rust instead of shell, invoked through a
cargo alias. It has **zero third-party dependencies** and ships nothing — it is a
developer tool, not part of the runtime.

```bash
cargo xtask <command> [args]          # via the alias in /.cargo/config.toml
cargo run -p xtask -- <command> [args] # equivalent, no alias needed
cargo xtask help
```

`xtask` is a workspace member but is excluded from `default-members`, so a bare
`cargo build` / `cargo test` skips it. It still compiles under
`cargo build --workspace`.

---

## `docs-check`

Validate repository documentation and workspace routing without loading opt-in historical context:

```bash
cargo xtask docs-check
```

The check fails on missing active repo-local Markdown file targets (inline and reference-style), active links into `docs/.agentarchive/`, mismatch between Cargo workspace packages and the canonical `crates/AGENTS.md` index, missing root routing to that index, or undocumented non-default workspace members. Heading-fragment validity is not checked. Transient worktrees, the append-only event ledger, and historical archive folders are intentionally excluded.

## `ci`

Run the single executable manifest shared by local development and CI:

```bash
cargo xtask ci                 # docs, build, test, Clippy, format, cargo-deny
cargo xtask ci --dry-run       # print the command manifest + CI-only lanes
cargo xtask ci --skip-deny     # CI matrix; cargo-deny runs in its own job
```

The command fails fast. GitHub Actions calls this manifest on macOS and Ubuntu instead of duplicating individual Rust commands in YAML.

## `clear-webrtc-cache`

Un-poisons the **libwebrtc download-cache** so the next build re-fetches
libwebrtc cleanly.

```bash
cargo xtask clear-webrtc-cache              # clear, then tell you how to rebuild
cargo xtask clear-webrtc-cache --rebuild    # clear, then rebuild ocean-call (debug)
cargo xtask clear-webrtc-cache --rebuild --release   # ...rebuild release too
```

### The bug it fixes

`ocean-call`'s `livekit-tap` feature pulls the native `livekit` client, which
depends on `webrtc-sys` / `webrtc-sys-build`. That build script downloads a
prebuilt libwebrtc archive into a persistent **scratch** dir under `target/`.

If the download is interrupted — Ctrl-C, dropped network, an OOM kill — the
scratch dir is left **existing but incomplete**. On the next build the script
sees the directory, early-returns a false "success", and never re-fetches. The
link step then fails with:

```text
error: could not find native static library `webrtc`, perhaps an -L flag is missing?
```

The historical manual fix:

```bash
rm -rf target/release/build/scratch-* \
       target/release/build/webrtc-sys-* \
       target/release/.fingerprint/webrtc-sys-*
```

`clear-webrtc-cache` does exactly that, but more completely:

- sweeps **both** the `debug` and `release` profiles,
- removes all three artifact families:
  - `build/scratch-*` — webrtc-sys-build's download-cache / scratch dir (the
    actual poisoning site),
  - `build/webrtc-sys-*` — the build-script output dir,
  - `.fingerprint/webrtc-sys-*` — the cargo fingerprint that otherwise convinces
    cargo the crate is already built (so it won't re-run the build script),
- respects `CARGO_TARGET_DIR` (absolute or relative to the workspace root); falls
  back to `<workspace_root>/target`,
- is a safe no-op when nothing matches — it prints `Nothing to clear` and exits 0,
- prints every directory it removes.

### When to reach for it

You pulled `livekit-tap` (or `xai-tts`, which implies it) and a build that used
to work now fails with `could not find native static library webrtc` — typically
right after an interrupted build. Run it, then rebuild:

```bash
cargo xtask clear-webrtc-cache
cargo build -p ocean-call --features livekit-tap
```

---

## Adding a new task

1. Add a module under `xtask/src/` (e.g. `src/my_task.rs`).
2. `mod my_task;` and a match arm in `src/main.rs`.
3. Document the flags in `print_usage()` and here.

Keep xtask dependency-free where practical — it should never be able to break a
workspace build.
