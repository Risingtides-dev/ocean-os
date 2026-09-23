#!/usr/bin/env bash
# Teammate onboarding for Ocean on a macOS arm64 machine (docs/TEAM_ONBOARDING.md).
#
# Idempotent; safe to re-run. What it does, in order, stopping at the first
# failure:
#   1. Preflight: macOS arm64; bun + gh (brew-installed if missing); gh logged in
#      with the read:packages scope. It never starts an interactive login itself —
#      it fails closed and prints the exact command to run first.
#   2. ~/.npmrc: the @risingtides-dev scope on GitHub Packages, one line each,
#      never duplicated; the auth line is refreshed from `gh auth token`.
#   3. bun add -g @risingtides-dev/ocean (npm fallback) — ocean, ocean-daemon,
#      ocean-mcp, ocean-update.
#   4. ~/.config/ocean-rs/federation.env — the Bedrock origin, 0600 in a 0700
#      directory. A file that already carries a real owner credential is left
#      alone unless --force.
#   5. ~/.config/ocean-rs/member.toml — member_id = "<--member>" (and the optional
#      display_name), 0600. This is who you are in every room, on every host:
#      the daemon's GET /v1/identity, ocean-mcp, and the surface all read it.
#      There is no default; the daemon never posts as your shell user. An
#      existing file naming someone else is left alone unless --force.
#   6. LaunchAgent dev.risingtides.ocean-daemon — the package daemon is copied to
#      the immutable ~/.local/libexec/ocean-daemon/current artifact and run by
#      the repo launcher (deploy/ocean-daemon.sh) from $HOME with OCEAN_YOLO=1
#      and OCEAN_MODEL; then /health is awaited. Same label, launcher, neutral
#      cwd, and log path as ops/install-ocean-daemon.sh, minus the cargo build.
#   7. ocean-mcp setup, and `claude mcp add` when Claude Code is on PATH.
#   8. Prints the remaining manual checklist (provider login, invite redeem,
#      doctor).
#
# It never prints a token, and it does NOT redeem an invite: the code is yours.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LABEL="dev.risingtides.ocean-daemon"
DOMAIN="gui/$(id -u)"
PLIST_DST="$HOME/Library/LaunchAgents/$LABEL.plist"
LIBEXEC="$HOME/.local/libexec/ocean-daemon"
LAUNCHER_SRC="$REPO/deploy/ocean-daemon.sh"
PKG="@risingtides-dev/ocean"
NPMRC="$HOME/.npmrc"
SCOPE_LINE="@risingtides-dev:registry=https://npm.pkg.github.com"
TOKEN_KEY="//npm.pkg.github.com/:_authToken="
DEFAULT_BEDROCK_URL="https://ocean-bedrock-production.up.railway.app"
CONFIG_DIR="${OCEAN_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/ocean-rs}"
ENV_FILE="${OCEAN_FEDERATION_ENV_FILE:-$CONFIG_DIR/federation.env}"
MEMBER_FILE="$CONFIG_DIR/member.toml"
HEALTH_URL="http://127.0.0.1:4780/health"
LOG_PATH="/private/tmp/ocean-daemon.log"
# A coworker's daemon is a MEMBER node: the Bedrock origin only, no owner
# bearer (crates/ocean-daemon/src/room_federation.rs, FederationConfig::resolve
# accepts an origin-only file; the launcher logs `federation=on (file, member)`).
# Invite redemption never uses a bearer; only the room owner's daemon carries
# one, written by ops/set-ocean-federation.sh. This comment line is how a re-run
# tells a file it wrote from one carrying a real credential.
MEMBER_MARKER="# ocean-onboard: member node (origin only, no owner bearer)"

# bun's global bin dir and Homebrew, ahead of whatever the caller's shell had.
export PATH="$HOME/.bun/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

usage() {
  cat <<USAGE
Usage:
  ops/onboard-teammate.sh --model MODEL --member ID [--display-name NAME]
                          [--bedrock-url URL] [--force] [--dry-run]

  --model MODEL        Required. The model the supervised daemon starts with
                       (OCEAN_MODEL). Ask the operator which alias to use; once the
                       daemon is up, \`curl -s 127.0.0.1:4780/v1/models\` lists the
                       registry and you can switch with /model in the ocean TUI.
  --member ID          Required. Your member id — the username the operator put in
                       the surface's users.json (smaths, ecfromthedc). Written to
                       ~/.config/ocean-rs/member.toml; it is who you are in every
                       room from every host. Letters, digits, . _ @ - only.
  --display-name NAME  Optional cosmetic name beside the id ("Eric").
  --bedrock-url URL    Bedrock origin (scheme + host only). Default:
                       $DEFAULT_BEDROCK_URL
  --force              Replace a federation.env that carries a real owner
                       credential, a member.toml that names someone else, and a
                       supervised daemon artifact that was built from a repo
                       checkout.
  --dry-run            Print every mutating step instead of doing it.
USAGE
}

say() { printf '==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
fail() { echo "FATAL: $1" >&2; exit "${2:-1}"; }
run() {
  if [[ $DRY -eq 1 ]]; then
    printf '    [dry-run] %s\n' "$*"
  else
    "$@"
  fi
}

valid_federation_origin() {
  local candidate="$1" authority port
  if [[ ! "$candidate" =~ ^https://[A-Za-z0-9.-]+(:[0-9]{1,5})?$ && ! "$candidate" =~ ^http://(127\.0\.0\.1|localhost)(:[0-9]{1,5})?$ ]]; then
    return 1
  fi
  authority="${candidate#*://}"
  [[ "$authority" == *:* ]] || return 0
  port="${authority##*:}"
  (( 10#$port <= 65535 ))
}

MODEL=""; MEMBER_ID=""; DISPLAY_NAME=""; BEDROCK_URL="$DEFAULT_BEDROCK_URL"; FORCE=0; DRY=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --model) MODEL="${2:-}"; shift 2 ;;
    --member) MEMBER_ID="${2:-}"; shift 2 ;;
    --display-name) DISPLAY_NAME="${2:-}"; shift 2 ;;
    --bedrock-url) BEDROCK_URL="${2:-}"; shift 2 ;;
    --force) FORCE=1; shift ;;
    --dry-run) DRY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; fail "unknown argument: $1" 64 ;;
  esac
done
[[ -n "$MODEL" ]] || { usage >&2; fail "--model is required (the daemon never defaults to a model)" 64; }
[[ "$MODEL" =~ ^[A-Za-z0-9][A-Za-z0-9._:/-]*$ ]] || fail "--model '$MODEL' is not a model alias" 64
[[ -n "$MEMBER_ID" ]] || { usage >&2; fail "--member is required (there is no default identity: the daemon never posts as your shell user)" 64; }
# The same character set the daemon (crates/ocean-daemon/src/identity.rs) and
# ocean-mcp accept, so the file this writes is one they will read.
[[ "$MEMBER_ID" =~ ^[A-Za-z0-9._@-]+$ ]] || fail "--member '$MEMBER_ID' may only contain letters, digits, . _ @ -" 64
if [[ -n "$DISPLAY_NAME" ]]; then
  [[ ${#DISPLAY_NAME} -le 80 && "$DISPLAY_NAME" != *'"'* && "$DISPLAY_NAME" != *$'\n'* ]] || fail "--display-name must be at most 80 characters with no double quotes or newlines" 64
fi
valid_federation_origin "$BEDROCK_URL" || fail "--bedrock-url must be an https origin with nothing after the host (http only for 127.0.0.1/localhost)" 64
[[ -f "$LAUNCHER_SRC" ]] || fail "launcher $LAUNCHER_SRC is missing; run this from an ocean-os checkout" 66

# ── 1. preflight ───────────────────────────────────────────────────────────
say "[1/8] preflight"
[[ "$(uname -s)" == "Darwin" && "$(uname -m)" == "arm64" ]] \
  || fail "this kit is for macOS on Apple silicon; got $(uname -s) $(uname -m)" 64
note "macOS $(sw_vers -productVersion 2>/dev/null || echo '?') arm64"

ensure_tool() {
  local bin="$1" formula="$2"
  if command -v "$bin" >/dev/null 2>&1; then
    note "$bin: $(command -v "$bin")"
    return 0
  fi
  command -v brew >/dev/null 2>&1 \
    || fail "$bin is missing and Homebrew is not installed; install Homebrew (https://brew.sh) or $bin, then re-run" 69
  say "installing $bin (brew install $formula)"
  run brew install "$formula"
}
ensure_tool bun oven-sh/bun/bun
ensure_tool gh gh

if ! gh auth status >/dev/null 2>&1; then
  fail "gh is not logged in. Run: gh auth login   (accept the KINGMAKER-SYSTEMS org invite in the browser), then: gh auth refresh -s read:packages   and re-run" 78
fi
if ! gh auth status 2>&1 | grep -q 'read:packages'; then
  fail "the gh token lacks the read:packages scope, which installing $PKG from GitHub Packages needs. Run: gh auth refresh -s read:packages   then re-run" 78
fi
note "gh: logged in with read:packages"
if gh api user/memberships/orgs/KINGMAKER-SYSTEMS --jq '.state' >/dev/null 2>&1; then
  note "GitHub org KINGMAKER-SYSTEMS: member"
else
  note "GitHub org KINGMAKER-SYSTEMS: not a member yet, or the invite is still pending (ask the operator)"
fi

# ── 2. ~/.npmrc ────────────────────────────────────────────────────────────
say "[2/8] ~/.npmrc — $SCOPE_LINE"
ensure_npmrc() {
  local token tmp
  token="$(gh auth token 2>/dev/null || true)"
  [[ -n "$token" ]] || fail "gh auth token returned nothing" 78
  if [[ $DRY -eq 1 ]]; then
    note "[dry-run] would write the scope line and refresh the //npm.pkg.github.com auth line in $NPMRC (0600)"
    return 0
  fi
  [[ -e "$NPMRC" ]] || : > "$NPMRC"
  chmod 600 "$NPMRC"
  tmp="$(mktemp "$HOME/.npmrc.XXXXXX")"
  # Keep every unrelated line; drop any earlier copy of these two so a re-run
  # never duplicates them and a rotated gh token replaces the stale one.
  grep -v -F -e "$SCOPE_LINE" -e "$TOKEN_KEY" "$NPMRC" > "$tmp" || true
  printf '%s\n' "$SCOPE_LINE" "${TOKEN_KEY}${token}" >> "$tmp"
  unset token
  chmod 600 "$tmp"
  mv -f "$tmp" "$NPMRC"
  note "wrote $NPMRC (0600; the auth line holds your gh token — never paste it anywhere)"
}
ensure_npmrc

# ── 3. the package ─────────────────────────────────────────────────────────
say "[3/8] installing $PKG@latest"
if command -v bun >/dev/null 2>&1; then
  run bun add -g "$PKG@latest"
else
  run npm install -g "$PKG@latest"
fi
if [[ $DRY -eq 0 ]]; then
  for bin in ocean ocean-daemon ocean-mcp ocean-update; do
    command -v "$bin" >/dev/null 2>&1 \
      || fail "$bin is not on PATH after the install; is $HOME/.bun/bin on your PATH? (bun add -g puts binaries there)" 70
    note "$bin: $(command -v "$bin")"
  done
fi

# ── 4. federation.env ──────────────────────────────────────────────────────
say "[4/8] federation.env — $ENV_FILE"
write_federation_env() {
  local has_credential=0 has_marker=0 url_matches=0 tmp
  if [[ -e "$ENV_FILE" ]]; then
    grep -q '^OCEAN_FEDERATION_OWNER_TOKEN\(_KEYCHAIN\)\{0,1\}=' "$ENV_FILE" && has_credential=1
    grep -qxF "$MEMBER_MARKER" "$ENV_FILE" && has_marker=1
    grep -qx "OCEAN_FEDERATION_URL=$BEDROCK_URL" "$ENV_FILE" && url_matches=1
    if [[ $has_credential -eq 1 && $FORCE -eq 0 ]]; then
      note "already carries an owner credential (ops/set-ocean-federation.sh territory); leaving it untouched — --force replaces it"
      return 0
    fi
    if [[ $has_marker -eq 1 && $url_matches -eq 1 ]]; then
      run chmod 700 "$CONFIG_DIR"
      run chmod 600 "$ENV_FILE"
      note "already set for $BEDROCK_URL (0600)"
      return 0
    fi
  fi
  if [[ $DRY -eq 1 ]]; then
    note "[dry-run] would write OCEAN_FEDERATION_URL=$BEDROCK_URL (origin only, member node) at 0600 in a 0700 $CONFIG_DIR"
    return 0
  fi
  (
    umask 077
    mkdir -p "$CONFIG_DIR"
    chmod 700 "$CONFIG_DIR"
    tmp="$(mktemp "$CONFIG_DIR/.federation.env.XXXXXX")"
    {
      echo "# Written by ops/onboard-teammate.sh (docs/TEAM_ONBOARDING.md). Read by the daemon launcher at start."
      echo "# Owner-only (0600). Member node: the Bedrock origin only, no owner bearer —"
      echo "# this daemon joins rooms by invite and cannot bootstrap rooms as their owner."
      echo "$MEMBER_MARKER"
      echo "OCEAN_FEDERATION_URL=$BEDROCK_URL"
    } > "$tmp"
    chmod 600 "$tmp"
    mv -f "$tmp" "$ENV_FILE"
  )
  note "wrote $ENV_FILE (0600 in a 0700 dir): OCEAN_FEDERATION_URL=$BEDROCK_URL"
}
write_federation_env

# ── 5. member.toml — who you are ───────────────────────────────────────────
say "[5/8] member.toml — $MEMBER_FILE (member_id = \"$MEMBER_ID\")"
write_member_toml() {
  local current tmp
  if [[ -e "$MEMBER_FILE" ]]; then
    current="$(sed -n 's/^[[:space:]]*member_id[[:space:]]*=[[:space:]]*"\{0,1\}\([A-Za-z0-9._@-]*\)"\{0,1\}.*$/\1/p' "$MEMBER_FILE" | head -n1)"
    if [[ "$current" == "$MEMBER_ID" ]]; then
      run chmod 600 "$MEMBER_FILE"
      note "already names $MEMBER_ID (0600)"
      return 0
    fi
    if [[ -n "$current" && $FORCE -eq 0 ]]; then
      fail "$MEMBER_FILE already names '$current', not '$MEMBER_ID'. One person per daemon: re-run with --force only if this machine is really yours." 64
    fi
  fi
  if [[ $DRY -eq 1 ]]; then
    note "[dry-run] would write member_id = \"$MEMBER_ID\"${DISPLAY_NAME:+ and display_name = \"$DISPLAY_NAME\"} at 0600 in a 0700 $CONFIG_DIR"
    return 0
  fi
  (
    umask 077
    mkdir -p "$CONFIG_DIR"
    chmod 700 "$CONFIG_DIR"
    tmp="$(mktemp "$CONFIG_DIR/.member.toml.XXXXXX")"
    {
      echo "# Written by ops/onboard-teammate.sh (docs/TEAM_ONBOARDING.md)."
      echo "# Who this daemon's human is, on every host: GET /v1/identity, ocean-mcp, and the"
      echo "# surface read this. One key = \"value\" per line; nothing else belongs here."
      echo "member_id = \"$MEMBER_ID\""
      [[ -n "$DISPLAY_NAME" ]] && echo "display_name = \"$DISPLAY_NAME\""
      true
    } > "$tmp"
    chmod 600 "$tmp"
    mv -f "$tmp" "$MEMBER_FILE"
  )
  note "wrote $MEMBER_FILE (0600): member_id = \"$MEMBER_ID\"${DISPLAY_NAME:+, display_name = \"$DISPLAY_NAME\"}"
}
write_member_toml

# ── 6. the supervised daemon ───────────────────────────────────────────────
say "[6/8] LaunchAgent $LABEL (package daemon, neutral cwd \$HOME, OCEAN_YOLO=1, OCEAN_MODEL=$MODEL)"
package_version() {
  local pkg_json="$1"
  [[ -f "$pkg_json" ]] || { echo unknown; return 0; }
  sed -n 's/^[[:space:]]*"version":[[:space:]]*"\([^"]*\)".*/\1/p' "$pkg_json" | head -n 1 | grep . || echo unknown
}
render_plist() {
  # Mirrors deploy/dev.risingtides.ocean-daemon.plist: same label, launcher copy,
  # neutral cwd, KeepAlive/RunAtLoad, throttle, log path, and retry policy. The
  # package daemon has no repo to build from, so the template's rustup/cargo PATH
  # entries give way to bun's bin dir. Federation values never live here; the
  # launcher reads federation.env right before exec.
  cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <!-- Rendered by ops/onboard-teammate.sh for a package-installed Ocean daemon.
       Same supervision shape as deploy/dev.risingtides.ocean-daemon.plist
       (ops/install-ocean-daemon.sh); the binary under ~/.local/libexec is a copy
       of the @risingtides-dev/ocean package's ocean-daemon. -->
  <key>Label</key>
  <string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$LIBEXEC/launch.sh</string>
  </array>
  <!-- Neutral cwd: the daemon refuses to start inside a git repo. -->
  <key>WorkingDirectory</key>
  <string>$HOME</string>
  <key>KeepAlive</key>
  <true/>
  <key>RunAtLoad</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>$LOG_PATH</string>
  <key>StandardErrorPath</key>
  <string>$LOG_PATH</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>OCEAN_YOLO</key>
    <string>1</string>
    <key>OCEAN_MODEL</key>
    <string>$MODEL</string>
    <key>OCEAN_RETRY_MAX_ATTEMPTS</key>
    <string>8</string>
    <key>OCEAN_RETRY_BASE_BACKOFF_MS</key>
    <string>400</string>
    <key>OCEAN_RETRY_MAX_BACKOFF_MS</key>
    <string>8000</string>
    <key>PATH</key>
    <string>$HOME/.bun/bin:$HOME/.cargo/bin:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
</dict>
</plist>
PLIST
}
install_launch_agent() {
  local pkg_daemon version dest tmp_link current_target
  if [[ $DRY -eq 1 && ! -x "$(command -v ocean-daemon 2>/dev/null || true)" ]]; then
    note "[dry-run] would copy the package ocean-daemon to $LIBEXEC/ocean-daemon-pkg-<version>, flip $LIBEXEC/current,"
    note "[dry-run] install $LAUNCHER_SRC as $LIBEXEC/launch.sh, render $PLIST_DST, and bootstrap $DOMAIN/$LABEL"
    return 0
  fi
  pkg_daemon="$(readlink -f "$(command -v ocean-daemon)")"
  [[ -x "$pkg_daemon" ]] || fail "could not resolve the package ocean-daemon binary" 70
  version="$(package_version "$(dirname "$pkg_daemon")/../package.json")"
  dest="$LIBEXEC/ocean-daemon-pkg-$version"
  # A `current` that is not a package artifact was published by
  # ops/install-ocean-daemon.sh from a repo build (the operator's machine). This
  # kit must not silently replace that provenance.
  if [[ -L "$LIBEXEC/current" ]]; then
    current_target="$(readlink "$LIBEXEC/current")"
    case "$(basename "$current_target")" in
      ocean-daemon-pkg-*) ;;
      *) [[ $FORCE -eq 1 ]] || fail "$LIBEXEC/current points at a repo-built artifact ($(basename "$current_target")); this machine is operated by ops/install-ocean-daemon.sh. Re-run with --force to replace it with the package build." 64 ;;
    esac
  fi
  if [[ $DRY -eq 1 ]]; then
    note "[dry-run] would publish $pkg_daemon -> $dest and flip $LIBEXEC/current"
    note "[dry-run] would install $LAUNCHER_SRC -> $LIBEXEC/launch.sh and render $PLIST_DST"
    note "[dry-run] would bootout/bootstrap/enable/kickstart $DOMAIN/$LABEL and wait for $HEALTH_URL"
    return 0
  fi
  mkdir -p "$LIBEXEC" "$HOME/Library/LaunchAgents"
  # Immutable artifact + atomic `current` flip, exactly as the installer does, so
  # `ocean-update` never hot-swaps the running daemon underneath launchd.
  install -m 0755 "$pkg_daemon" "$dest"
  tmp_link="$LIBEXEC/.current.$$"
  ln -s "$dest" "$tmp_link"
  mv -f "$tmp_link" "$LIBEXEC/current"
  # Keep the three newest artifacts; `current` always survives via its target.
  find "$LIBEXEC" -maxdepth 1 -type f -name 'ocean-daemon-*' -exec stat -f '%m %N' {} + 2>/dev/null \
    | sort -rn | tail -n +4 | cut -d' ' -f2- | while read -r old; do
    [[ "$(readlink "$LIBEXEC/current")" == "$old" ]] || rm -f "$old"
  done
  note "published $dest (current -> $(readlink "$LIBEXEC/current"))"
  install -m 0755 "$LAUNCHER_SRC" "$LIBEXEC/launch.sh"
  note "launcher copy -> $LIBEXEC/launch.sh"

  render_plist > "$PLIST_DST"
  plutil -lint "$PLIST_DST" >/dev/null
  if grep -q 'OCEAN_FEDERATION' "$PLIST_DST"; then
    fail "$PLIST_DST carries a federation key; the launcher is the only channel" 70
  fi
  note "rendered $PLIST_DST"

  # Guarded restart: bootout is asynchronous, so wait for the job to vanish
  # before bootstrapping (ops/install-ocean-daemon.sh, TASK-22).
  launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
  for _ in $(seq 1 50); do
    launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1 || break
    sleep 0.2
  done
  if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    fail "$LABEL did not tear down within 10s; refusing to race bootstrap. Inspect: launchctl print $DOMAIN/$LABEL" 75
  fi
  if ! launchctl bootstrap "$DOMAIN" "$PLIST_DST" 2>/dev/null; then
    sleep 1
    launchctl bootstrap "$DOMAIN" "$PLIST_DST" \
      || fail "bootstrap failed twice — the daemon is DOWN. Recover with: launchctl bootstrap $DOMAIN $PLIST_DST   then check $HEALTH_URL" 70
  fi
  launchctl enable "$DOMAIN/$LABEL"
  launchctl kickstart -k "$DOMAIN/$LABEL"
  for _ in $(seq 1 30); do
    curl -fsS -m 2 "$HEALTH_URL" >/dev/null 2>&1 && break
    sleep 1
  done
  if ! curl -fsS -m 2 "$HEALTH_URL" >/dev/null 2>&1; then
    fail "launchd job installed but nothing is serving $HEALTH_URL after 30s. Inspect: launchctl print $DOMAIN/$LABEL; tail $LOG_PATH" 70
  fi
  note "healthy: $(curl -fsS -m 2 "$HEALTH_URL" 2>/dev/null | sed -n 's/.*"rev":"\([^"]*\)".*/rev \1/p' | head -n 1)"
  if grep -q 'federation=on (file)' "$LOG_PATH" 2>/dev/null; then
    note "federation: on (file) — the launcher accepted $ENV_FILE"
  else
    note "federation: the launcher did not report 'federation=on (file)'; check $LOG_PATH for a 'federation OFF' reason code"
  fi
}
install_launch_agent

# ── 7. ocean-mcp ───────────────────────────────────────────────────────────
say "[7/8] ocean-mcp — the Ocean skill and MCP server for Claude Code / Codex"
if command -v ocean-mcp >/dev/null 2>&1; then
  run ocean-mcp setup
  if command -v claude >/dev/null 2>&1; then
    if claude mcp get ocean >/dev/null 2>&1; then
      note "claude: MCP server 'ocean' already registered"
    else
      run claude mcp add --scope user ocean -- "$(command -v ocean-mcp)" serve
    fi
  else
    note "claude is not on PATH; when it is: claude mcp add --scope user ocean -- $(command -v ocean-mcp) serve"
  fi
else
  note "[dry-run] ocean-mcp is not installed yet; would run: ocean-mcp setup, then claude mcp add --scope user ocean -- ocean-mcp serve"
fi

# ── 8. what is left for you ────────────────────────────────────────────────
say "[8/8] done. Next, in this order:"
cat <<CHECKLIST
    1. Sign in to a model provider (browser flow):   ocean      then type   /login
       Models the daemon knows (aliases for --model / /model):
         curl -s 127.0.0.1:4780/v1/models
    2. Join the team room — paste the code from the operator's invite:
         curl -s -X POST http://127.0.0.1:4780/v1/rooms/persistent/invites/redeem \\
           -H 'content-type: application/json' -d '{"code":"<code>"}'
       A good answer carries "state":"live" (or "connecting", then live) and "room_key".
    3. Verify:                                        ocean-mcp doctor
       (its 'member id' line must say $MEMBER_ID) and in Claude Code ask it to
       read the room (the ocean_room_read tool); post a hello with ocean_room_post.

    Daemon health:   curl -fsS $HEALTH_URL
    Who you are:     curl -fsS http://127.0.0.1:4780/v1/identity   (member_id "$MEMBER_ID", source "member.toml")
    Daemon log:      tail -f $LOG_PATH      (look for 'federation=on (file)')
    Codex:           add to ~/.codex/config.toml —
                       [mcp_servers.ocean]
                       command = "$(command -v ocean-mcp 2>/dev/null || echo ocean-mcp)"
                       args = ["serve"]
    Updating later:  ocean-update, then re-run this script — the supervised daemon
                     is an immutable copy and is not hot-swapped by the package.
CHECKLIST
