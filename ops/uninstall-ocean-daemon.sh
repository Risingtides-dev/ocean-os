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
WD_LABEL="dev.risingtides.ocean-daemon-watchdog"
PLIST_DST="$HOME/Library/LaunchAgents/$LABEL.plist"
WD_DST="$HOME/Library/LaunchAgents/$WD_LABEL.plist"
DOMAIN="gui/$(id -u)"

echo "==> booting out $DOMAIN/$WD_LABEL"
launchctl bootout "$DOMAIN/$WD_LABEL" 2>/dev/null || echo "    (watchdog was not loaded)"

echo "==> booting out $DOMAIN/$LABEL"
launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || echo "    (job was not loaded)"

echo "==> removing $PLIST_DST and $WD_DST"
rm -f "$PLIST_DST" "$WD_DST"

echo "==> done. The daemon is no longer supervised."
echo "    Verify it stopped:  lsof -nP -iTCP:4780 -sTCP:LISTEN   (should print nothing)"
