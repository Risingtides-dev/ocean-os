#!/usr/bin/env bash
# Reviewed install/activation procedure for the daemon's federation credentials
# (spec line 0.6; docs/OPERATIONS.md "Enable federation").
#
# What it does, in order, stopping at the first failure:
#   1. Writes the untracked, owner-only federation.env that the supervised
#      daemon's launcher (deploy/ocean-daemon.sh, installed as launch.sh) reads
#      right before it execs the daemon. The bearer never appears on this
#      script's command line, in shell history, or in any log: it is read from
#      a file you name, from stdin, or left in the Keychain and only referenced.
#   2. Lints both plists and proves neither carries a federation value.
#   3. Restarts the supervised daemon through the guarded
#      bootout -> wait -> bootstrap -> enable -> kickstart sequence that
#      ops/install-ocean-daemon.sh uses, then waits for /health.
#   4. Optionally verifies a real federated room from the daemon's own
#      snapshot, reading nothing but access.state.
#
# Run it after turns drain: the kickstart drops whatever turn is in flight.
set -euo pipefail
umask 077

LABEL="dev.risingtides.ocean-daemon"
DOMAIN="gui/$(id -u)"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLIST_SRC="$REPO/deploy/$LABEL.plist"
PLIST_DST="$HOME/Library/LaunchAgents/$LABEL.plist"
ENV_FILE="${OCEAN_FEDERATION_ENV_FILE:-${OCEAN_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/ocean-rs}/federation.env}"
HEALTH_URL="${OCEAN_HEALTH_URL:-http://127.0.0.1:4780/health}"

usage() {
  cat <<USAGE
Usage:
  ops/set-ocean-federation.sh --url https://bedrock.example (--token-file PATH | --token-stdin | --keychain SERVICE) [--no-restart] [--verify-room KEY]
  ops/set-ocean-federation.sh --off [--no-restart]

  --url URL            Bedrock origin: scheme and host only, nothing after the host.
  --token-file PATH    Read the owner bearer from PATH (the file is not modified).
  --token-stdin        Read the owner bearer from standard input (one line).
  --keychain SERVICE   Do not store the bearer in the file; the launcher reads it at
                       start from the login Keychain item with that service name and
                       this account name. Add it with:
                         security add-generic-password -a "\$USER" -s SERVICE -w
  --off                Remove the federation file; the daemon restarts with federation off.
  --no-restart         Write the file and lint the plists, but do not touch launchd.
  --verify-room KEY    After the restart, poll the room's snapshot until access.state is live.

The bearer is never accepted on the command line.
USAGE
}

fail() { echo "FATAL: $*" >&2; exit 1; }

url=""; token_file=""; token_stdin=0; keychain=""; off=0; restart=1; verify_room=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --url) url="${2:-}"; shift 2 ;;
    --token-file) token_file="${2:-}"; shift 2 ;;
    --token-stdin) token_stdin=1; shift ;;
    --keychain) keychain="${2:-}"; shift 2 ;;
    --off) off=1; shift ;;
    --no-restart) restart=0; shift ;;
    --verify-room) verify_room="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; fail "unknown argument: $1" ;;
  esac
done

if [[ $off -eq 0 ]]; then
  [[ -n "$url" ]] || { usage >&2; fail "--url is required"; }
  if [[ ! "$url" =~ ^https://[A-Za-z0-9.-]+(:[0-9]{1,5})?$ && ! "$url" =~ ^http://(127\.0\.0\.1|localhost)(:[0-9]{1,5})?$ ]]; then
    fail "--url must be an https origin with nothing after the host (http is allowed for 127.0.0.1 and localhost only)"
  fi
  n=0; [[ -n "$token_file" ]] && n=$((n+1)); [[ $token_stdin -eq 1 ]] && n=$((n+1)); [[ -n "$keychain" ]] && n=$((n+1))
  [[ $n -eq 1 ]] || { usage >&2; fail "give exactly one of --token-file, --token-stdin, --keychain"; }
fi

# ── 1. the owner-only file ────────────────────────────────────────────────
mkdir -p "$(dirname "$ENV_FILE")"
chmod 700 "$(dirname "$ENV_FILE")" 2>/dev/null || true
if [[ $off -eq 1 ]]; then
  rm -f "$ENV_FILE"
  echo "==> [1/3] removed $ENV_FILE (federation will be off after the restart)"
else
  token=""
  if [[ -n "$token_file" ]]; then
    [[ -f "$token_file" ]] || fail "--token-file $token_file is not a file"
    IFS= read -r token < "$token_file" || true
  elif [[ $token_stdin -eq 1 ]]; then
    IFS= read -r token || true
  fi
  if [[ -z "$keychain" ]]; then
    [[ -n "$token" ]] || fail "the bearer is empty"
    [[ "$token" =~ [[:space:]] ]] && fail "the bearer holds whitespace"
  fi
  tmp="$(mktemp "$(dirname "$ENV_FILE")/.federation.env.XXXXXX")"
  {
    echo "# Written by ops/set-ocean-federation.sh. Read only by the daemon's launcher."
    echo "# Owner-only (0600). Never copy these values into any plist or shell profile."
    echo "OCEAN_FEDERATION_URL=$url"
    if [[ -n "$keychain" ]]; then echo "OCEAN_FEDERATION_OWNER_TOKEN_KEYCHAIN=$keychain"; else echo "OCEAN_FEDERATION_OWNER_TOKEN=$token"; fi
  } > "$tmp"
  unset token
  chmod 600 "$tmp"
  mv -f "$tmp" "$ENV_FILE"
  echo "==> [1/3] wrote $ENV_FILE (0600, $(if [[ -n "$keychain" ]]; then echo "bearer in Keychain item '$keychain'"; else echo "bearer in the file"; fi))"
fi

# ── 2. the plists carry nothing ───────────────────────────────────────────
for plist in "$PLIST_SRC" "$PLIST_DST"; do
  [[ -f "$plist" ]] || continue
  if command -v plutil >/dev/null 2>&1; then plutil -lint "$plist" >/dev/null || fail "$plist does not lint"; fi
  if grep -q "OCEAN_FEDERATION" "$plist"; then fail "$plist carries a federation key; remove it, the launcher is the only channel"; fi
done
echo "==> [2/3] plists lint and carry no federation value"

[[ $restart -eq 1 ]] || { echo "==> --no-restart: launchd untouched; the next daemon start picks the file up"; exit 0; }

# ── 3. guarded restart (same sequence as ops/install-ocean-daemon.sh) ─────
command -v launchctl >/dev/null 2>&1 || fail "launchctl is not available here; use --no-restart on a non-macOS host"
[[ -f "$PLIST_DST" ]] || fail "$PLIST_DST is missing; run ops/install-ocean-daemon.sh first"
echo "==> [3/3] restarting $LABEL in $DOMAIN (drops any turn in flight)"
launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
for _ in $(seq 1 20); do
  launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1 || break
  sleep 0.5
done
if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
  fail "$LABEL did not tear down within 10s; refusing to race bootstrap. Inspect: launchctl print $DOMAIN/$LABEL"
fi
if ! launchctl bootstrap "$DOMAIN" "$PLIST_DST" 2>/dev/null; then
  sleep 1
  launchctl bootstrap "$DOMAIN" "$PLIST_DST" || fail "bootstrap failed twice — THE DAEMON IS DOWN. Recover by hand: launchctl bootstrap $DOMAIN $PLIST_DST"
fi
launchctl enable "$DOMAIN/$LABEL"
launchctl kickstart -k "$DOMAIN/$LABEL"
for _ in $(seq 1 60); do
  if curl -fsS "$HEALTH_URL" >/dev/null 2>&1; then echo "==> healthy: $HEALTH_URL"; break; fi
  sleep 0.5
done
curl -fsS "$HEALTH_URL" >/dev/null 2>&1 || fail "$HEALTH_URL did not answer within 30s after the restart; read /private/tmp/ocean-daemon.log"
if [[ -n "$verify_room" ]]; then
  base="${HEALTH_URL%/health}"
  state=""
  for _ in $(seq 1 60); do
    state="$(curl -fsS "$base/v1/rooms/persistent/$verify_room/snapshot?before_seq=18446744073709551615&limit=1" 2>/dev/null | sed -n 's/.*"access":{[^}]*"state":"\([a-z]*\)".*/\1/p' | head -1)"
    [[ "$state" == "live" ]] && break
    sleep 1
  done
  [[ "$state" == "live" ]] || fail "room $verify_room access.state is '${state:-unreadable}', not live, after 60s"
  echo "==> verified: room $verify_room access.state=live (nothing else was read or printed)"
fi
echo "==> done. Record the verification in events.md with the daemon revision from /health."
