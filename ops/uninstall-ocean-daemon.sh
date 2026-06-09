#!/usr/bin/env bash
# Stop + unsupervise the Ocean daemon (reverse of install-ocean-daemon.sh).
# Boots the job out of launchd and removes the installed plist. Leaves the repo
# and the built binary untouched.
#
# After this, the daemon is NO LONGER supervised — it will NOT come back on crash
# or reboot until you either re-install or hand-launch it again
# (cd <repo> && OCEAN_YOLO=1 ./target/release/ocean-daemon).
set -euo pipefail

LABEL="dev.risingtides.ocean-daemon"
PLIST_DST="$HOME/Library/LaunchAgents/$LABEL.plist"
DOMAIN="gui/$(id -u)"

echo "==> booting out $DOMAIN/$LABEL"
launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || echo "    (job was not loaded)"

echo "==> removing $PLIST_DST"
rm -f "$PLIST_DST"

echo "==> done. The daemon is no longer supervised."
echo "    Verify it stopped:  lsof -nP -iTCP:4780 -sTCP:LISTEN   (should print nothing)"
