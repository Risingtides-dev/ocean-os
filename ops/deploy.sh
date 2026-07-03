#!/usr/bin/env bash
# One-command daemon redeploy: build from MAIN, restart, verify health.
#
# Codifies the operator's manual redeploy loop (ops/README.md) so no step can
# be skipped under pressure:
#   1. HARD-fail unless the repo is on main with a clean tree (deploy-from-main
#      rule — install-ocean-daemon.sh only warns; this script refuses).
#   2. cargo build -p ocean-daemon --release
#   3. launchctl kickstart -k  (drops any in-flight turn — that's the deal)
#   4. Poll /health until the NEW process answers, or fail loudly.
#
# First-time install (plists not yet bootstrapped)? Use ops/install-ocean-daemon.sh.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LABEL="dev.risingtides.ocean-daemon"
DOMAIN="gui/$(id -u)"
HEALTH_URL="http://127.0.0.1:4780/health"

export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/usr/local/bin:/opt/homebrew/bin:$PATH"

branch="$(git -C "$REPO" rev-parse --abbrev-ref HEAD)"
if [[ "$branch" != "main" ]]; then
  echo "FATAL: on branch '$branch' — deploy from MAIN only (git checkout main)." >&2
  exit 1
fi
if [[ -n "$(git -C "$REPO" status --porcelain)" ]]; then
  echo "FATAL: working tree not clean — commit or stash before deploying." >&2
  exit 1
fi

echo "==> [1/3] building ocean-daemon (release) at $(git -C "$REPO" rev-parse --short HEAD)"
( cd "$REPO" && cargo build -p ocean-daemon --release )

echo "==> [2/3] kickstarting $LABEL"
launchctl kickstart -k "$DOMAIN/$LABEL"

echo "==> [3/3] verifying $HEALTH_URL"
for i in $(seq 1 15); do
  sleep 2
  if out="$(curl -fsS -m 3 "$HEALTH_URL" 2>/dev/null)"; then
    echo "    healthy after $((i * 2))s: $out"
    echo "==> deploy complete."
    exit 0
  fi
done
echo "FATAL: daemon not healthy after 30s — check: tail -50 /private/tmp/ocean-daemon.log" >&2
exit 1
