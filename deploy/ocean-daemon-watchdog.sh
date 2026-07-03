#!/usr/bin/env bash
# Health watchdog for the Ocean daemon.
#
# launchd's KeepAlive only respawns on EXIT — a hung-but-alive daemon (deadlock,
# stuck event loop) never restarts. This script closes that gap: launchd runs it
# every 60s (StartInterval, see dev.risingtides.ocean-daemon-watchdog.plist);
# it curls /health and kickstarts the daemon if the check fails.
#
# /health is the truth (a 404 on another path ≠ down — ops/README.md).
set -uo pipefail

LABEL="dev.risingtides.ocean-daemon"
DOMAIN="gui/$(id -u)"
HEALTH_URL="http://127.0.0.1:4780/health"

# ponytail: 2 tries 5s apart before restarting — enough to ride out a slow GC
# pause without flapping; tune if false positives ever show up in the log.
for attempt in 1 2; do
  if curl -fsS -m 5 "$HEALTH_URL" >/dev/null 2>&1; then
    exit 0
  fi
  [[ "$attempt" == 1 ]] && sleep 5
done

echo "$(date '+%Y-%m-%d %H:%M:%S') WATCHDOG: $HEALTH_URL failed twice — kickstarting $LABEL"
launchctl kickstart -k "$DOMAIN/$LABEL"
