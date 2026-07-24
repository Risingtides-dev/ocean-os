#!/usr/bin/env bash
# Install + supervise the Ocean daemon under launchd (OCEAN-253).
#
# Idempotent. Safe to re-run after a pull/rebuild. What it does:
#   1. Requires MAIN, then builds the daemon binary (release). The installer
#      fails closed on every other branch (operator rule: never deploy/run the
#      daemon from a feature branch).
#   2. Copies the LaunchAgent plist into ~/Library/LaunchAgents/.
#   3. Bootstraps + enables + kickstarts the job in the per-user GUI domain.
#
# This DOES touch the live launchd on this box AND will (re)start the daemon —
# run it intentionally. The daemon owns live sessions; a kickstart drops whatever
# turn is in flight. Supersedes the old hand-launch
# (cd <repo> && OCEAN_YOLO=1 ./target/release/ocean-daemon).
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LABEL="dev.risingtides.ocean-daemon"
PLIST_SRC="$REPO/deploy/$LABEL.plist"
PLIST_DST="$HOME/Library/LaunchAgents/$LABEL.plist"
DOMAIN="gui/$(id -u)"

export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/usr/local/bin:/opt/homebrew/bin:$PATH"

# --- build-from-main guard (operator rule) -------------------------------------
# Fail closed if the repo isn't on main. A warning-and-continue path can deploy
# a feature-branch binary while every operator contract claims production is
# main-built, so branch mismatch is a hard configuration error.
branch="$(git -C "$REPO" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')"
# TASK-15: a detached worktree whose HEAD is main CONTENT satisfies the
# build-from-main rule — deploys routinely run from clean throwaway worktrees
# at origin/main precisely so the dev checkout's state can't leak into
# production. Only content that main doesn't contain is fatal.
#
# The containment check runs for EVERY branch name, `main` included. Being on a
# branch called `main` proves nothing: a shared dev checkout whose local main
# sits ahead of origin will happily build commits that were never pushed, never
# reviewed, and never seen by CI. That is not hypothetical — the supervised
# daemon on the primary host reported rev 40cf0d6d-dirty, an unpushed local-main
# commit, for days. `--is-ancestor` is the check that actually enforces the rule.
git -C "$REPO" fetch origin main --quiet 2>/dev/null || true
if ! git -C "$REPO" merge-base --is-ancestor HEAD origin/main 2>/dev/null; then
  echo "FATAL: repo at $REPO is on '$branch' and HEAD is not contained in origin/main." >&2
  echo "       Operator rule: build/deploy the daemon from MAIN content only." >&2
  echo "       Local commits ahead of origin/main count as 'not contained' — push" >&2
  echo "       and merge them first, or deploy from a worktree at origin/main." >&2
  exit 64 # EX_USAGE
fi
if [[ -n "$(git -C "$REPO" status --porcelain --untracked-files=no 2>/dev/null)" ]]; then
  echo "FATAL: repo at $REPO has tracked modifications — deploy trees must be clean." >&2
  exit 64 # EX_USAGE
fi

echo "==> [1/3] building ocean-daemon (release, legacy-chromium) from '$branch'"
# INTERIM: the supervised daemon keeps the legacy Chromium browser backend
# (`legacy-chromium` feature) so agent browsing stays live while the
# OceanWebKit engine program (docs/specs/2026-07-19-ocean-webkit-browser-program.md)
# replaces it. Default builds compile no chromiumoxide; this flag is the
# deliberate operator-visible exception for the production daemon. Drop it when
# the OceanWebKit browser host ships.
( cd "$REPO" && cargo build -p ocean-daemon --release --features legacy-chromium )

BIN="$REPO/target/release/ocean-daemon"
if [[ ! -x "$BIN" ]]; then
  echo "FATAL: build did not produce an executable at $BIN" >&2
  exit 1
fi

# --- TASK-7: publish an immutable versioned artifact -------------------------
# The launcher (deploy/ocean-daemon.sh) execs ~/.local/libexec/ocean-daemon/current,
# never the repo's target/release output, so checkout builds can't silently
# become the running daemon. Copy this build to a rev-named path and flip the
# `current` symlink atomically (symlink-at-temp-path + rename).
LIBEXEC="$HOME/.local/libexec/ocean-daemon"
rev="$(git -C "$REPO" describe --always --dirty --abbrev=12 2>/dev/null || echo unknown)"
DEST_BIN="$LIBEXEC/ocean-daemon-$rev"
mkdir -p "$LIBEXEC"
install -m 0755 "$BIN" "$DEST_BIN"
TMP_LINK="$LIBEXEC/.current.$$"
ln -s "$DEST_BIN" "$TMP_LINK"
mv -f "$TMP_LINK" "$LIBEXEC/current"
# Convenience path used by hand-launches and the ocean TUI.
mkdir -p "$HOME/.local/bin"
ln -sfn "$LIBEXEC/current" "$HOME/.local/bin/ocean-daemon"
# Keep the three newest artifacts; `current` always survives via its target.
ls -t "$LIBEXEC"/ocean-daemon-* 2>/dev/null | tail -n +4 | while read -r old; do
  [[ "$(readlink "$LIBEXEC/current")" == "$old" ]] || rm -f "$old"
done
echo "==> published $DEST_BIN (current -> $(readlink "$LIBEXEC/current"))"

# --- TASK-15: publish the launcher script beside the binary artifacts --------
# launchd execs this COPY, never the repo's deploy/ocean-daemon.sh, so a dev
# checkout's working-tree state cannot affect supervision.
install -m 0755 "$REPO/deploy/ocean-daemon.sh" "$LIBEXEC/launch.sh"
echo "==> published launcher copy -> $LIBEXEC/launch.sh"

echo "==> [2/3] rendering plist template -> $PLIST_DST"
mkdir -p "$HOME/Library/LaunchAgents"
# The committed plist is machine-neutral; render __OCEAN_HOME__ here so no
# operator-specific absolute path ever lives in the repo (TASK-15).
sed "s|__OCEAN_HOME__|$HOME|g" "$PLIST_SRC" > "$PLIST_DST"
plutil -lint "$PLIST_DST"
if grep -q "__OCEAN_HOME__" "$PLIST_DST"; then
  echo "FATAL: plist rendering left unexpanded placeholders." >&2
  exit 70 # EX_SOFTWARE
fi

echo "==> [3/3] (re)bootstrapping launchd job $LABEL in $DOMAIN"
# Tear down any previous instance so this is a clean (re)install.
#
# TASK-22: `bootout` is ASYNCHRONOUS. Bootstrapping immediately after it races
# the teardown and fails with "Bootstrap failed: 5: Input/output error" — and
# because the bootout itself already succeeded, that leaves the daemon DOWN
# while the installer exits nonzero. (This took production offline on the
# 07-20 deploy.) Wait for the job to actually disappear before bootstrapping.
launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
for _ in $(seq 1 50); do
  launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1 || break
  sleep 0.2
done
if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
  echo "FATAL: $LABEL did not tear down within 10s; refusing to race bootstrap." >&2
  echo "       Inspect: launchctl print $DOMAIN/$LABEL" >&2
  exit 75 # EX_TEMPFAIL
fi

# Bootstrap, with one retry — and never exit quietly leaving the box daemonless.
if ! launchctl bootstrap "$DOMAIN" "$PLIST_DST" 2>/dev/null; then
  sleep 1
  if ! launchctl bootstrap "$DOMAIN" "$PLIST_DST"; then
    echo "FATAL: bootstrap failed twice — THE DAEMON IS DOWN." >&2
    echo "       Recover with:" >&2
    echo "         launchctl bootstrap $DOMAIN $PLIST_DST" >&2
    echo "       Then verify: curl -fsS http://127.0.0.1:4780/health" >&2
    exit 70 # EX_SOFTWARE
  fi
fi
launchctl enable "$DOMAIN/$LABEL"
# Force an immediate (re)start so we don't wait for the next event.
launchctl kickstart -k "$DOMAIN/$LABEL"

# Prove the install actually produced a serving daemon: an installer that
# returns 0 while the box has no daemon is the failure mode TASK-22 exists to
# kill.
for _ in $(seq 1 30); do
  curl -fsS -m 2 http://127.0.0.1:4780/health >/dev/null 2>&1 && break
  sleep 1
done
if ! curl -fsS -m 2 http://127.0.0.1:4780/health >/dev/null 2>&1; then
  echo "FATAL: launchd job installed but nothing is serving /health after 30s." >&2
  echo "       Inspect: launchctl print $DOMAIN/$LABEL; tail /private/tmp/ocean-daemon.log" >&2
  exit 70 # EX_SOFTWARE
fi

echo
echo "==> done. status:"
launchctl print "$DOMAIN/$LABEL" 2>/dev/null | grep -E 'state|pid|program|path =' | sed 's/^/    /' || true
echo
echo "    Check it's listening:   lsof -nP -iTCP:4780 -sTCP:LISTEN"
echo "    Health check:           curl -fsS http://127.0.0.1:4780/health && echo"
echo "    Tail logs:              tail -f /private/tmp/ocean-daemon.log"
