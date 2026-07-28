# Ocean OS operations

Status: concise current runbook. For exhaustive configuration, endpoint, and
troubleshooting detail, use
[`OCEAN_RUNTIME_OPERATOR_GUIDE.md`](OCEAN_RUNTIME_OPERATOR_GUIDE.md).

## Prerequisites

- Rust 1.88 or newer (`rust-version` is enforced by CI).
- Provider credentials for the selected model, unless using a test fixture.
- macOS system dependencies from the existing Cargo configuration for the
  default operated path; hosted Ubuntu feature lanes install GLib headers.

Never print, commit, or copy auth files into a worktree. Credential resolution
is owned by `ocean-providers`; inspect its current source and the extended
operator guide rather than maintaining another key-name table here.

## Build

```bash
cargo build --workspace --release
```

Narrow binaries:

```bash
cargo build -p ocean-daemon --release
cargo build -p ocean-tui --release
cargo build -p ocean-cli --release
```

The binaries are:

- `target/release/ocean-daemon`
- `target/release/ocean-tui`
- `target/release/ocean-rs`

## Run locally

The daemon refuses to start inside a Git repository by design. Build in the
checkout, then launch the absolute binary from a neutral directory:

```bash
repo="$(pwd)"
(
  cd "$HOME"
  OCEAN_MODEL=<model-alias> "$repo/target/release/ocean-daemon"
)
```

Default endpoint: `http://127.0.0.1:4780`.

```bash
curl -fsS http://127.0.0.1:4780/health
curl -fsS http://127.0.0.1:4780/ready
curl -fsS http://127.0.0.1:4780/metrics | head
```

In another terminal:

```bash
./target/release/ocean-rs health
./target/release/ocean-rs prompt "Reply with: pong"
./target/release/ocean-tui
```

The TUI startup chooser creates a new session or enters the resume, editor, or
project-graph flow. An explicit TUI session argument bypasses discovery; session
persistence still belongs to the daemon.

## Model and provider selection

Select a model through the supported configuration or `OCEAN_MODEL`. There is
no safe reason for entry-point docs to freeze a copied model list; the current
catalog and aliases live in `ocean-providers::known_models` and are exposed by
the daemon's model routes/TUI picker.

A model is ready only when its provider and credentials resolve. `/health`
proves liveness; use `/ready` and the model readiness surfaces when validating a
real turn.

## Permissions

Mutating tools are gated unless the operator explicitly enables a trusted mode.
The CLI's non-interactive default is deny; use an explicit permission mode when
a scripted run is intended to approve tools. Do not change this posture merely
to make an automation stop prompting.

## macOS supervised daemon

The supported service is `dev.risingtides.ocean-daemon`.

Before installing, make cleanliness and synchronization explicit operator preconditions:

```bash
git fetch origin
test "$(git branch --show-current)" = main
test -z "$(git status --porcelain)"
test "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)"
./ops/install-ocean-daemon.sh
```

The installer itself refuses a non-`main` branch, builds release code, installs the plist, and bootstraps launchd with a neutral working directory. It does not prove that `main` is clean or synchronized with `origin/main`; the preflight above does.

After a reviewed code change has merged to `main`:

```bash
git fetch origin
git merge --ff-only origin/main
cargo build --workspace --release
curl -fsS http://127.0.0.1:4780/metrics | \
  awk '$1 == "ocean_turns_in_flight" { print }'
launchctl kickstart -k "gui/$(id -u)/dev.risingtides.ocean-daemon"
curl -fsS http://127.0.0.1:4780/health
```

Wait for `ocean_turns_in_flight 0` before restarting. Restart only the named
LaunchAgent; do not use a broad `pkill`.

Inspect service state and logs:

```bash
launchctl print "gui/$(id -u)/dev.risingtides.ocean-daemon"
tail -n 200 /private/tmp/ocean-daemon.log
```

Use the actual plist/script paths if local configuration differs; the tracked
installer and `ops/README.md` are authoritative.

## Install the TUI

Feature branches must build and test the TUI normally. Installation is a
separate delivery gate: after review and merge, update a clean checkout whose
HEAD is contained in `origin/main`, then run:

```bash
./ops/install-ocean-tui.sh
```

The installer runs the locked release build, publishes an immutable
revision-named artifact under `~/.local/libexec/ocean-tui/`, verifies its ad-hoc
code signature, and atomically updates `~/.local/bin/ocean`. This avoids stale
or untraceable binaries and avoids overwriting a running Mach-O in place.

After installation, run a real multi-second PTY/TUI smoke rather than treating
`--help` as launch proof. A TUI change is not shipped merely because a release
binary exists under a worktree's `target/` directory.

## Verification

Docs-only:

```bash
cargo xtask docs-check
git diff --check
```

Canonical local merge gate:

```bash
cargo xtask ci
```

Build compatibility:

```bash
cargo xtask ci --compatibility
cargo +1.88.0 xtask ci --msrv
```

CI runs supported-feature/release checks on macOS and Ubuntu, a pinned Rust 1.88
lane on Ubuntu, and `cargo-deny` separately. A local run does not replace those
hosted lanes.

## Recovery rules

- If `/health` fails, inspect launchd state and the daemon error log before
  restarting repeatedly.
- If the daemon reports a repository cwd, fix the service working directory;
  do not bypass the startup guard with `OCEAN_ALLOW_REPO_CWD=1`.
- If a surface appears stale, compare the daemon revision and client revision;
  session/runtime bugs can affect every client simultaneously.
- If a turn is active, preserve it or wait for it to drain before deployment.
- Roll back by rebuilding and deploying a reviewed known-good `main` revision;
  never mix binaries from an uncommitted checkout.
