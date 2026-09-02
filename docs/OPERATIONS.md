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

## Rooms and federation

Rooms are daemon-local by default: a room, its transcript, roster, artifacts
and attachments live in `rooms.db` beside the config dir (`OCEAN_CONFIG_DIR`,
else `$XDG_CONFIG_HOME/ocean-rs`, else `~/.config/ocean-rs`; `OCEAN_DB_PATH`
overrides the full path). Federation is what lets a room span daemons: invites,
the Bedrock room stream, and the container workspace lane. It is off unless the
supervised daemon carries two environment variables, and as of 2026-09-01 the
tracked plist carries neither, so the operated daemon runs rooms local-only and
any room that ever held a Bedrock credential sits in `recovering`. The
mechanics are in `OCEAN_RUNTIME_OPERATOR_GUIDE.md` under "Federated-room
Bedrock bridge"; this section is the operator's order of operations. The
finish line it serves is `docs/specs/2026-09-01-ocean-rooms-definition-of-done.md`.

### Enable federation

1. Bedrock must be reachable over HTTPS at its origin with its federation
   schema applied: `npm run db:check` there must report `roomMembersReady`
   and `roomComputeReady`, and `operatorRoomsReady` once ocean-bedrock #117 is
   deployed. Production today is `https://ocean-bedrock-production.up.railway.app`.
2. Mint the daemon's owner bearer on Bedrock. Today that is an admin token
   (`npm run token:create` there); Bedrock's own handoff warns that path scopes
   do not constrain an admin token, so it is full-instance authority, and
   ocean-bedrock #117 adds the operator path that will replace it. Treat the
   value as a production secret.
3. Set, on the supervised daemon only:
   - `OCEAN_FEDERATION_URL` — the Bedrock origin, nothing after the host.
   - `OCEAN_FEDERATION_OWNER_TOKEN` — the bearer from step 2.

   The installer renders `deploy/dev.risingtides.ocean-daemon.plist` and
   substitutes only `__OCEAN_HOME__`; there is no untracked env file yet. Add
   the two keys to the RENDERED plist at
   `~/Library/LaunchAgents/dev.risingtides.ocean-daemon.plist` after
   `./ops/install-ocean-daemon.sh`, never to the tracked template, and repeat
   after every reinstall because the installer overwrites it. Teaching the
   installer to merge a local, untracked env file is the follow-up that
   removes this step.
4. Restart the named LaunchAgent after turns drain, exactly as in the
   supervised-daemon section above.
5. Verify, in this order. First the revision:

   ```bash
   curl -fsS http://127.0.0.1:4780/health
   ```

   Then bootstrap one existing Local room as its Bedrock owner by minting an
   invite from the surface's room invite control, or by POSTing
   `/v1/rooms/persistent/<key>/invites` with the body the operator guide
   documents; the response is the only place the invite code and `onboard_url`
   appear. Then read the room's access state:

   ```bash
   curl -fsS "http://127.0.0.1:4780/v1/rooms/persistent/<key>/snapshot?before_seq=18446744073709551615&limit=1"
   ```

   `access.state` must move from `connecting` to `live` within one reconnect
   interval. A `recovering` that never turns `live` means the origin was
   rejected, the token is invalid, or Bedrock is down; the daemon moves every
   credentialed room there on bad configuration rather than leaving stale
   `live` chrome. Then post a message in that room: it answers 202 and reaches
   the transcript only when Bedrock's ordered stream confirms it, and the
   snapshot's `outbox` must drain to empty.
6. Record the verification in `events.md` with the daemon revision and the
   room key, as the 2026-08-31 install entry did. Until such an entry exists,
   line 0.6 of the rooms definition of done stays open.

### The workspace lane

The daemon proxies a federated room's Bedrock workspace under
`GET|POST /v1/rooms/persistent/{key}/workspace/{leaf}` with the room's own
credential, so no browser ever holds the bearer. It needs the room to be
federated (a credential in `rooms.db`) and Bedrock to run a compute driver with
its runtime Worker deployed. Bedrock's typed refusals (`workspace_absent`,
`repo_unbound`, `federation_unavailable`) are relayed as states, not errors.
Owner verbs (provision, destroy, repo bind and unbind, secrets set) forward
only for the actor that resolves to the credential's own principal. CI pulls
need a `GH_TOKEN` room secret set through `secrets/set`; Bedrock returns no
secret value on any route.

### Reading the bridge without metrics

There are no room or federation counters on `/metrics` yet (definition of done,
line 4.1). Until there are:

- `access.state` on the snapshot is the primary signal: `live` is caught up,
  `recovering` is replaying from the durable cursor or misconfigured, `revoked`
  means this principal's membership was removed and nothing is writable.
- `outbox` on the snapshot: pending rows that age while the state is `live`
  mean the sender is not being confirmed. The ordered SSE stream is the only
  confirmation rail; a ledger 201 never is.
- The daemon log (`/private/tmp/ocean-daemon.log`) carries the bridge's
  reconnects and refusals under the `room_federation` module; jittered
  reconnects cap at 60 s.
- Presence follows the SSE lease: a disconnect downgrades every projected
  member to Unavailable in the same access commit.

### Rollback

Remove the two variables from the rendered plist and kickstart. Local rooms are
untouched; credentialed rooms sit in `recovering` with honest chrome; nothing
is deleted. Re-adding the variables resumes from the persisted cursor.

### rooms.db migration rehearsal

The Phase 1 manifest's rollout gate 4 asks for a migration rehearsal on a copy
of a real `rooms.db`, including rollback. **As of 2026-09-01 this has not been
performed**: the only rehearsal entries in `events.md` concern `ocean-memory`.
Line 4.5 of the rooms definition of done stays open until an `events.md` entry
records one; writing this procedure down is not performing it. Procedure:

1. Copy the live store without stopping the daemon:
   `cp <config dir>/rooms.db /tmp/rooms-rehearsal.db`, using the config dir
   the running daemon actually uses (`launchctl print` shows its environment).
2. Run the candidate binary against the copy on a spare port with
   `OCEAN_DB_PATH=/tmp/rooms-rehearsal.db` and `OCEAN_BIND`, from the immutable
   artifact under `~/.local/libexec/ocean-daemon/`.
3. Through the candidate, list rooms and read one snapshot; the store applies
   its migrations on open, so a clean open plus readable rooms is the pass.
4. Rollback: stop the candidate and delete the copy. For a real upgrade,
   rollback is the previous immutable artifact plus the pre-upgrade copy from
   step 1, because the store's migrations are forward-only; the copy is the
   rollback.
5. Record both revisions and the room and message counts before and after in
   `events.md`.

### Bedrock ordering this daemon depends on

ocean-bedrock #117 makes Bedrock's `register()` write `room_members.operator_id`,
so Bedrock migration `db/013` must be applied to its production database before
any deploy containing #117, or legacy room registration (the path the owner
bootstrap above uses) answers 500. Bedrock's `npm run deploy:status` and
`npm run db:check` are the checks; this daemon cannot see either.

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
