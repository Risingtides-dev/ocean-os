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
the Bedrock room stream, and the container workspace lane.
`OCEAN_FEDERATION_URL` enables transport for rooms that already hold a Bedrock
credential; `OCEAN_FEDERATION_OWNER_TOKEN` separately lets this daemon bootstrap
an existing Local room as its Bedrock owner. Missing the owner token does not
disable existing credentialed rooms or invite redemption. Missing or invalid
transport configuration moves credentialed non-revoked rooms to `recovering`;
revoked rooms remain revoked. As of 2026-09-01 the tracked plist carries neither
variable, so the operated daemon cannot bootstrap Local rooms and cannot run
transport for any credentials already in `rooms.db`. The mechanics are in
`OCEAN_RUNTIME_OPERATOR_GUIDE.md` under "Federated-room
Bedrock bridge"; this section is the operator's order of operations. The
tracked completion state is the Ocean Rooms section of `../ROADMAP.md`; the
Phase 1 rollout gates remain authoritative in
`specs/2026-08-25-ocean-rooms-phase1-room-agent-authorization-manifest.md`.

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
4. Reload the edited plist after turns drain. `kickstart` alone restarts the
   already-loaded definition and will not load the new environment. Follow the
   same teardown guard as `ops/install-ocean-daemon.sh`, then bootstrap and
   kickstart the named job:

   ```bash
   plutil -lint "$HOME/Library/LaunchAgents/dev.risingtides.ocean-daemon.plist"
   launchctl bootout "gui/$(id -u)/dev.risingtides.ocean-daemon" 2>/dev/null || true
   for _ in $(seq 1 50); do
     launchctl print "gui/$(id -u)/dev.risingtides.ocean-daemon" >/dev/null 2>&1 || break
     sleep 0.2
   done
   ! launchctl print "gui/$(id -u)/dev.risingtides.ocean-daemon" >/dev/null 2>&1
   launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/dev.risingtides.ocean-daemon.plist"
   launchctl enable "gui/$(id -u)/dev.risingtides.ocean-daemon"
   launchctl kickstart -k "gui/$(id -u)/dev.risingtides.ocean-daemon"
   ```

   Stop if the teardown assertion or bootstrap fails; the installer documents
   the recovery command and health check for that state.
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
   room key, as the 2026-08-31 install entry did. The runbook is not evidence
   that federation was enabled; keep that operational state explicit in the
   Ocean Rooms roadmap or its accepted successor contract.

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

There are no room or federation counters on `/metrics` yet. Until there are:

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

Remove the two variables from the rendered plist, then repeat the guarded
`bootout` → wait → `bootstrap` → `enable` → `kickstart` sequence above so
launchd loads their removal. Local rooms are untouched; credentialed rooms sit
in `recovering` with honest chrome; nothing is deleted. Re-adding the variables
and reloading the plist resumes from the persisted cursor.

### rooms.db migration rehearsal

The Phase 1 manifest's rollout gate 4 asks for a migration rehearsal on a real
**pre-Phase-1 schema**, including rollback. The 2026-08-31 rehearsal in
`events.md` was useful but does not close this gate: its `rooms.db` source had
already been opened by Phase 1 and already carried the additive
`room_agent_bindings` and `room_agent_decisions` tables (both empty). The
downgrade portion exercised the memory migration and proved that an older
daemon ignored those empty additive tables; it did not prove that the candidate
creates the room-agent tables from the pre-Phase-1 schema or that rollback
restores a database without them. The gate remains open until a new
`events.md` entry records the proof below. Procedure:

1. Prefer a retained transactionally consistent backup taken before the Phase 1
   cutover. If none exists, reconstruct the old schema only in an isolated
   online backup of the live database: first require both Phase 1 tables to be
   empty, then drop `room_agent_decisions` before `room_agent_bindings`. Stop if
   either table contains data; that copy cannot honestly stand in for a
   pre-Phase-1 source. Create isolated config roots for both halves of the
   rehearsal; the candidate must not inherit live `titles.db`, extension
   projects, observatory, or any other daemon state:

   ```bash
   rooms_rehearsal_dir="$(mktemp -d /tmp/ocean-rooms-rehearsal.XXXXXX)"
   mkdir -p "$rooms_rehearsal_dir/candidate-config" "$rooms_rehearsal_dir/rollback-config"
   running_ocean_db_path="<OCEAN_DB_PATH from the running launchd job, or empty>"
   running_ocean_config_dir="<effective OCEAN_CONFIG_DIR or resolved default>"
   if [ -n "$running_ocean_db_path" ]; then
     live_rooms_db="$running_ocean_db_path"
   else
     live_rooms_db="$running_ocean_config_dir/rooms.db"
   fi
   test -f "$live_rooms_db"
   sqlite3 "$live_rooms_db" ".timeout 10000" \
     ".backup '$rooms_rehearsal_dir/candidate-config/rooms.db'"
   test "$(sqlite3 "$rooms_rehearsal_dir/candidate-config/rooms.db" \
     "SELECT coalesce(sum(rows),0) FROM (SELECT count(*) AS rows FROM room_agent_bindings UNION ALL SELECT count(*) FROM room_agent_decisions);")" = 0
   sqlite3 "$rooms_rehearsal_dir/candidate-config/rooms.db" \
     "DROP TABLE room_agent_decisions; DROP TABLE room_agent_bindings;"
   test "$(sqlite3 "$rooms_rehearsal_dir/candidate-config/rooms.db" \
     "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('room_agent_bindings','room_agent_decisions');")" = 0
   sqlite3 "$rooms_rehearsal_dir/candidate-config/rooms.db" "PRAGMA quick_check;"
   sqlite3 "$rooms_rehearsal_dir/candidate-config/rooms.db" \
     ".backup '$rooms_rehearsal_dir/pre-upgrade-rollback.db'"
   sqlite3 "$rooms_rehearsal_dir/pre-upgrade-rollback.db" "PRAGMA quick_check;"
   ```

   Resolve both placeholders from the running job, not from the operator shell:
   `launchctl print` shows its environment. Use its `OCEAN_DB_PATH` first; only
   when that variable is absent may the source fall back to `rooms.db` under the
   daemon's effective config dir. Replace the angle-bracket placeholders before
   running the command, and require both `quick_check` calls to print `ok`. When
   a retained pre-cutover backup exists, use SQLite's backup API to seed
   `candidate-config/rooms.db` from it, require the `sqlite_master` absence
   query above to return zero, and do not run either the row-count query or the
   two `DROP TABLE` statements: a genuine pre-Phase-1 database has no such
   tables to count. The row-count query belongs only to reconstruction from a
   current database, while its tables still exist and must be proven empty
   before they are dropped.
2. Resolve the exact candidate and previous immutable artifacts under
   `~/.local/libexec/ocean-daemon/`. The rollback artifact must be a revision
   from **before Phase 1**, not merely the artifact preceding the current
   release. Verify both are executable and run the candidate on a spare
   loopback port from the neutral rehearsal directory. Both the candidate and
   rollback binaries refuse to run from any Ocean repository worktree;
   `OCEAN_UNSUPERVISED=1` bypasses the loaded-supervisor guard, not the cwd
   guard.

   ```bash
   candidate_bin="$HOME/.local/libexec/ocean-daemon/ocean-daemon-<candidate-rev>"
   previous_bin="$HOME/.local/libexec/ocean-daemon/ocean-daemon-<pre-phase1-rev>"
   test -x "$candidate_bin" && test -x "$previous_bin"
   (cd "$rooms_rehearsal_dir" && \
     env -u OCEAN_FEDERATION_URL -u OCEAN_FEDERATION_OWNER_TOKEN \
       -u OCEAN_TITLES_DB_PATH -u OCEAN_PLUGINS_DIR \
       OCEAN_CONFIG_DIR="$rooms_rehearsal_dir/candidate-config" \
       OCEAN_DB_PATH="$rooms_rehearsal_dir/candidate-config/rooms.db" \
       OCEAN_BIND=127.0.0.1:18791 OCEAN_UNSUPERVISED=1 \
       "$candidate_bin")
   ```

   Keep this process in its own terminal. Only the copied `rooms.db` enters the
   isolated config root; no production config file is imported. Federation,
   title-database, and plugin-directory overrides are explicitly removed from
   the rehearsal process even if the operator shell exported them, so copied
   credentials, pending outbox rows, or pending redemptions cannot contact
   Bedrock, open the live title registry, or launch production plugins.
3. Through the candidate, list rooms and read one snapshot. Before stopping it,
   require SQLite to report both `room_agent_bindings` and
   `room_agent_decisions`; a clean open alone is not migration proof. Record the
   source room/message counts and the post-open table count.
4. Actually exercise rollback. Stop the candidate, restore the untouched
   pre-upgrade backup into the separate rollback config with SQLite's backup
   API, run `quick_check`, then start the **previous** immutable artifact on a
   second spare port with the same unsupervised and isolation controls:

   ```bash
   sqlite3 "$rooms_rehearsal_dir/pre-upgrade-rollback.db" \
     ".backup '$rooms_rehearsal_dir/rollback-config/rooms.db'"
   sqlite3 "$rooms_rehearsal_dir/rollback-config/rooms.db" "PRAGMA quick_check;"
   test "$(sqlite3 "$rooms_rehearsal_dir/rollback-config/rooms.db" \
     "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('room_agent_bindings','room_agent_decisions');")" = 0
   (cd "$rooms_rehearsal_dir" && \
     env -u OCEAN_FEDERATION_URL -u OCEAN_FEDERATION_OWNER_TOKEN \
       -u OCEAN_TITLES_DB_PATH -u OCEAN_PLUGINS_DIR \
       OCEAN_CONFIG_DIR="$rooms_rehearsal_dir/rollback-config" \
       OCEAN_DB_PATH="$rooms_rehearsal_dir/rollback-config/rooms.db" \
       OCEAN_BIND=127.0.0.1:18792 OCEAN_UNSUPERVISED=1 \
       "$previous_bin")
   ```

   List the same rooms and read the same snapshot through the previous binary;
   room and message counts must match the pre-upgrade backup. Stop it only after
   that read succeeds. This previous-binary read is the rollback proof; merely
   retaining or deleting a backup is not.
5. Record both exact revisions, the pre-open zero and post-open two-table
   results, both `quick_check` results, the room/message counts before candidate
   migration and after rollback, and the two isolated config paths in
   `events.md`. Remove the temporary rehearsal directory only after the evidence
   is captured.

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
