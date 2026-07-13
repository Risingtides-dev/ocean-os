# Ocean OS — Ops

## ocean-daemon is supervised by launchd (OCEAN-253)

The **daemon** (`crates/ocean-daemon`, built to `target/release/ocean-daemon`) is
the runtime/body: it owns the agent loop, tools, provider calls, **sessions**,
permissions, and events, and it listens on **`127.0.0.1:4780`**.

Previously it was **hand-launched** from the repo root —
`cd <repo> && OCEAN_YOLO=1 ./target/release/ocean-daemon`, logging to
`/private/tmp/ocean-daemon.log` — with **no supervision**. A reboot lost it
entirely, and a crash left it down until someone noticed. There was no
version-controlled supervision spec. **OCEAN-161** put the surface **proxy**
(`:8790`) under launchd but explicitly left the **daemon** out of scope; this
ticket (**OCEAN-253**) does the same for the daemon itself. Both clients (the TUI
and ocean-surface) steer this one daemon, so when it's down, everything is down —
which is exactly why it now runs under a launchd **LaunchAgent** that respawns it
on crash (`KeepAlive`) and starts it at login/reboot (`RunAtLoad`).

| Thing | Value |
|---|---|
| launchd label | `dev.risingtides.ocean-daemon` |
| Version-controlled plist | `deploy/dev.risingtides.ocean-daemon.plist` |
| Launcher it execs | `deploy/ocean-daemon.sh` |
| Installed plist path | `~/Library/LaunchAgents/dev.risingtides.ocean-daemon.plist` |
| Binary run | `target/release/ocean-daemon` (prebuilt, from **main**) |
| Working directory | neutral directory outside any git repo (`$HOME` by default; override with `OCEAN_DAEMON_CWD`) |
| Bind address | `127.0.0.1:4780` (binary default; env `OCEAN_BIND` to override) |
| Env | `OCEAN_YOLO=1` (matches the prior hand-launch) |
| Logs (stdout+stderr) | `/private/tmp/ocean-daemon.log` |

> The daemon must **never run from the repository root**. Turns carry their own
> cwd, and the startup guard rejects a repository cwd so an unbound fallback
> cannot silently bind to ocean-os. The launcher resolves the binary by absolute
> path, then runs from `$HOME` or `OCEAN_DAEMON_CWD`.
>
> ### Build from MAIN — always
> Per operator rule, **never build/deploy/run the daemon from a feature branch.**
> The LaunchAgent runs a **prebuilt** `target/release/ocean-daemon` — it does
> **not** recompile on respawn. So the binary on disk *is* the deployment. Before
> installing, be on `main` and build:
> ```bash
> git checkout main && git pull
> cargo build -p ocean-daemon --release
> ```
> `ops/install-ocean-daemon.sh` does the build for you and fails immediately if
> the checkout is not on `main`. **To ship new daemon code:** merge to main → rebuild from main →
> `launchctl kickstart -k` (see "Restart" below). A rebuild alone does nothing
> until the running process is restarted.

> ### On KeepAlive and health
> launchd's `KeepAlive` can't natively curl `/health`, so we use the right
> primitive: **respawn-on-exit** (`KeepAlive=true`). Any exit — panic, OOM,
> crash, reboot — brings the daemon straight back. `ThrottleInterval=10` bounds a
> crash loop (a startup-panicking bad binary respawns at most every 10s instead
> of pinning a core). The health endpoint is for *you* to check liveness
> (`curl http://127.0.0.1:4780/health`), not for launchd.

### Install / enable supervision

```bash
ops/install-ocean-daemon.sh
```

This requires `main`, builds the daemon (release), copies the plist into
`~/Library/LaunchAgents/`, then bootstraps + enables + kickstarts the job.
Idempotent — safe to re-run after a pull/rebuild. (Equivalent manual steps:
`cp deploy/dev.risingtides.ocean-daemon.plist ~/Library/LaunchAgents/` then
`launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.risingtides.ocean-daemon.plist`
and `launchctl enable gui/$(id -u)/dev.risingtides.ocean-daemon`.)

> **This supersedes the hand-launch.** Once installed, do **not** also run
> `./target/release/ocean-daemon` by hand — you'd get two daemons fighting over
> `:4780`. launchd owns it now.

### Check status

```bash
# Is it listening?
lsof -nP -iTCP:4780 -sTCP:LISTEN

# launchd's view (state, pid, last exit code):
launchctl print gui/$(id -u)/dev.risingtides.ocean-daemon | grep -E 'state|pid|last exit'

# Is it loaded at all? (note: a 404 on a path ≠ down; /health is the truth)
launchctl list | grep -i ocean

# Health endpoint:
curl -fsS http://127.0.0.1:4780/health && echo
```

### Restart / read logs

```bash
# Force a restart — e.g. after rebuilding the binary from main to ship new code:
launchctl kickstart -k gui/$(id -u)/dev.risingtides.ocean-daemon

# Tail logs:
tail -f /private/tmp/ocean-daemon.log
```

> A `kickstart -k` **drops whatever turn is in flight**. Persisted sessions remain
> on disk and can resume after restart, but the active turn itself is interrupted.
> Don't restart a live daemon mid-turn unless you mean to.

### Uninstall / stop supervision

```bash
ops/uninstall-ocean-daemon.sh
```

Boots the job out of launchd and removes the installed plist. The repo and the
built binary are left untouched. After this the daemon is unsupervised again
(won't survive crash/reboot) until you re-install.

> **Supervision state on this box drifts** — agents and operators have
> hand-launched things over time. Before assuming the daemon is (or isn't)
> supervised, re-verify with `launchctl list | grep -i ocean` and
> `ps aux | grep '[o]cean-daemon'`. Sibling: the surface proxy (`:8790`) is
> supervised separately in the `ocean-surface` repo (OCEAN-161,
> `dev.risingtides.ocean-surface-proxy`).
