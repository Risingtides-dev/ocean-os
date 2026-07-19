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
if [[ "$branch" != "main" ]]; then
  echo "FATAL: repo at $REPO is on branch '$branch', not 'main'." >&2
  echo "       Operator rule: build/deploy the daemon from MAIN only." >&2
  echo "       Switch the main checkout to main, update it, then re-run this installer." >&2
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

echo "==> [2/3] installing plist -> $PLIST_DST"
mkdir -p "$HOME/Library/LaunchAgents"
cp "$PLIST_SRC" "$PLIST_DST"
plutil -lint "$PLIST_DST"

echo "==> [3/3] (re)bootstrapping launchd job $LABEL in $DOMAIN"
# Tear down any previous instance so this is a clean (re)install.
launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
launchctl bootstrap "$DOMAIN" "$PLIST_DST"
launchctl enable "$DOMAIN/$LABEL"
# Force an immediate (re)start so we don't wait for the next event.
launchctl kickstart -k "$DOMAIN/$LABEL"

echo
echo "==> done. status:"
launchctl print "$DOMAIN/$LABEL" 2>/dev/null | grep -E 'state|pid|program|path =' | sed 's/^/    /' || true
echo
echo "    Check it's listening:   lsof -nP -iTCP:4780 -sTCP:LISTEN"
echo "    Health check:           curl -fsS http://127.0.0.1:4780/health && echo"
echo "    Tail logs:              tail -f /private/tmp/ocean-daemon.log"
