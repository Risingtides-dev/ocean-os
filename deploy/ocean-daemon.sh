#!/usr/bin/env bash
# Supervised launcher for the Ocean daemon (OCEAN-253).
#
# launchd execs the installed COPY of this script at
# ~/.local/libexec/ocean-daemon/launch.sh (rendered plist points there), so a
# dev checkout's working-tree state can never affect supervision (TASK-15).
# The repo copy is the source of truth; the installer refreshes the copy.
# It exec's the PREBUILT release binary with the production env. A supervised
# service must respawn fast and deterministically, so this script does NOT run
# `cargo build` — the binary is built once at install time (see
# ops/install-ocean-daemon.sh) from MAIN, per the operator's build-from-main
# rule. To pick up new code: rebuild from main, then kickstart -k (see ops/README).
#
# cwd is NEUTRAL ($HOME), not the repo. The daemon is workspace-agnostic
# (turns carry their own cwd); its startup guard refuses to boot from inside a
# git repo so unbound fallback turns don't bind to ocean-os (see main.rs,
# OCEAN_ALLOW_REPO_CWD). Pre-guard this mirrored the hand-launch
# (cd <repo> && OCEAN_YOLO=1 ./target/release/ocean-daemon); the guard made that
# cwd invalid, so we run from $HOME instead. BIN is resolved absolutely, so
# repo-cwd isn't needed to find the binary. Override via OCEAN_DAEMON_CWD.
set -euo pipefail

# Toolchain + common bins on PATH (launchd starts with a minimal PATH). The
# daemon shells out to tools (git, ripgrep, etc.) for its own tool calls, so a
# sane PATH matters even though this script doesn't compile anything.
# ${HOME:-} so an unset HOME degrades to a system PATH instead of tripping
# `set -u` before the clearer NEUTRAL_CWD diagnostics below can run.
export PATH="${HOME:-}/.rustup/toolchains/stable-aarch64-apple-darwin/bin:${HOME:-}/.cargo/bin:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin:$PATH"

# TASK-7: the supervised binary is an IMMUTABLE installed artifact, never the
# repo's mutable target/release/ output. A cargo build in the checkout (any
# branch) must not be able to silently become the running daemon at the next
# restart — that is exactly how the -dirty health revs happened. The installer
# copies each build to a versioned path and atomically flips `current`.
BIN="${OCEAN_DAEMON_BIN:-${HOME:-}/.local/libexec/ocean-daemon/current}"
if [[ ! -x "$BIN" ]]; then
  echo "FATAL: no installed daemon at $BIN." >&2
  echo "       Run ops/install-ocean-daemon.sh (from MAIN) to build, install a" >&2
  echo "       versioned artifact, and flip the 'current' symlink atomically." >&2
  echo "       The repo's target/release/ output is deliberately NOT launched." >&2
  exit 127
fi

# Production env. Mirrors the prior hand-launch exactly; override via the plist's
# EnvironmentVariables block or by exporting before launch.
#   OCEAN_YOLO=1            -> operator default: tools run without per-call gating.
#   OCEAN_BIND (optional)   -> defaults to 127.0.0.1:4780 inside the binary.
#   OCEAN_ASSISTANTS_DIR    -> optional; defaults to ~/.config/ocean-rs/assistants.
#   OCEAN_PROMPT_CAPTURE_DIR -> optional owner-only local JSON request captures;
#                               includes private prompt/transcript/tool content.
export OCEAN_YOLO="${OCEAN_YOLO:-1}"

# Run from a NEUTRAL cwd so the startup guard's repo-cwd check passes and the
# unbound-turn fallback anchor is harmless (home, not ocean-os).
#
# Guarded explicitly rather than leaning on `${..:-$HOME}` under `set -u`: if
# HOME is unset/empty (LaunchDaemon context, odd session bootstraps) or
# OCEAN_DAEMON_CWD points at a missing dir, fail with a clear FATAL line
# instead of a cryptic bash error inside a 10s KeepAlive crash loop.
NEUTRAL_CWD="${OCEAN_DAEMON_CWD:-${HOME:-}}"
if [[ -z "$NEUTRAL_CWD" ]]; then
  echo "FATAL: no neutral cwd — HOME is unset/empty and OCEAN_DAEMON_CWD is not set." >&2
  echo "       Set OCEAN_DAEMON_CWD to a directory outside any git repo." >&2
  exit 78 # EX_CONFIG
fi
if [[ ! -d "$NEUTRAL_CWD" ]]; then
  echo "FATAL: neutral cwd '$NEUTRAL_CWD' does not exist (check OCEAN_DAEMON_CWD)." >&2
  exit 78 # EX_CONFIG
fi
# ── Federation credentials: the daemon-specific secret loader (spec line 0.6) ──
# OCEAN_FEDERATION_URL and OCEAN_FEDERATION_OWNER_TOKEN reach the supervised
# daemon through an untracked, owner-only file that this launcher reads right
# before exec. An explicit process environment wins wholesale; the file is a
# fallback only when both variables are absent. Neither source enters the
# tracked template, rendered plist, launchd domain (`launchctl setenv` is never
# used), or this log.
# launchd runs this launcher on every start, so a fresh login or reboot goes
# through the same path as an installer run. A file that fails any custody
# check is refused WHOLE and the daemon starts with federation OFF, the state
# it had before the file existed; every refusal names the reason and never the
# contents. The daemon independently enforces the same file custody when it is
# run without this launcher.
OCEAN_FEDERATION_ENV_FILE="${OCEAN_FEDERATION_ENV_FILE:-${OCEAN_CONFIG_DIR:-${XDG_CONFIG_HOME:-${HOME:-}/.config}/ocean-rs}/federation.env}"
federation="off"
federation_refuse() {
  echo "==> ocean-daemon: federation OFF — private configuration refused: $1" >&2
  unset OCEAN_FEDERATION_URL OCEAN_FEDERATION_OWNER_TOKEN
  federation="off"
}
federation_load() {
  local f="$1" parent parent_mode parent_owner mode owner inode opened_owner opened_inode line n=0 key value url="" token="" keychain=""
  local federation_fd parse_error=""
  local url_seen=0 token_seen=0 keychain_seen=0
  parent="$(dirname "$f")"
  if [[ -L "$parent" || ! -d "$parent" ]]; then federation_refuse "unsafe_parent"; return; fi
  if [[ -L "$f" || ! -f "$f" ]]; then federation_refuse "not_regular"; return; fi
  # GNU stat and BSD stat spell this differently, and GNU's `-f` is a
  # filesystem query that would print a block of text into the capture, so
  # pick the flavour first instead of falling through one to the other.
  if stat -c '%a' / >/dev/null 2>&1; then
    read -r parent_mode parent_owner < <(stat -c '%a %u' "$parent" 2>/dev/null || echo '? ?')
    read -r mode owner inode < <(stat -c '%a %u %i' "$f" 2>/dev/null || echo '? ? ?')
  else
    read -r parent_mode parent_owner < <(stat -f '%Lp %u' "$parent" 2>/dev/null || echo '? ?')
    read -r mode owner inode < <(stat -f '%Lp %u %i' "$f" 2>/dev/null || echo '? ? ?')
  fi
  if [[ ! "$parent_mode" =~ ^[0-7]+$ ]]; then
    federation_refuse "unsafe_parent"; return
  fi
  if [[ "$parent_owner" != "$(id -u)" ]] || (( (8#$parent_mode & 8#022) != 0 )); then
    federation_refuse "unsafe_parent"; return
  fi
  if [[ "$mode" != "600" ]]; then federation_refuse "unsafe_mode"; return; fi
  if [[ "$owner" != "$(id -u)" ]]; then federation_refuse "foreign_owner"; return; fi
  # Open exactly once, then prove that descriptor is the inode just validated.
  # The owner-only, non-group/world-writable parent prevents another account
  # from renaming an entry between the pathname checks and this open; the inode
  # comparison also refuses a same-user replacement in that interval. Parsing
  # below reads this descriptor, never the pathname again.
  if ! exec {federation_fd}<"$f"; then federation_refuse "open_failed"; return; fi
  if stat -c '%a' / >/dev/null 2>&1; then
    read -r opened_owner opened_inode < <(stat -Lc '%u %i' "/dev/fd/$federation_fd" 2>/dev/null || echo '? ?')
  else
    read -r opened_owner opened_inode < <(stat -f '%u %i' "/dev/fd/$federation_fd" 2>/dev/null || echo '? ?')
  fi
  if [[ "$opened_owner" != "$owner" || "$opened_inode" != "$inode" ]]; then
    exec {federation_fd}<&-
    federation_refuse "changed_during_open"; return
  fi
  while IFS= read -r line || [[ -n "$line" ]]; do
    n=$((n + 1)); line="${line%$'\r'}"
    [[ -z "${line// /}" || "$line" == \#* ]] && continue
    key="${line%%=*}"; value="${line#*=}"
    case "$key" in
      OCEAN_FEDERATION_URL) [[ "$url_seen" -eq 0 ]] || { parse_error="duplicate_entry"; break; }; url_seen=1; url="$value" ;;
      OCEAN_FEDERATION_OWNER_TOKEN) [[ "$token_seen" -eq 0 ]] || { parse_error="duplicate_entry"; break; }; token_seen=1; token="$value" ;;
      OCEAN_FEDERATION_OWNER_TOKEN_KEYCHAIN) [[ "$keychain_seen" -eq 0 ]] || { parse_error="duplicate_entry"; break; }; keychain_seen=1; keychain="$value" ;;
      *) parse_error="unsupported_entry"; break ;;
    esac
  done <&"$federation_fd"
  exec {federation_fd}<&-
  if [[ -n "$parse_error" ]]; then federation_refuse "$parse_error"; return; fi
  if [[ ! "$url" =~ ^https://[A-Za-z0-9.-]+(:[0-9]{1,5})?$ && ! "$url" =~ ^http://(127\.0\.0\.1|localhost)(:[0-9]{1,5})?$ ]]; then
    federation_refuse "origin is invalid"; return
  fi
  if [[ -n "$token" && -n "$keychain" ]]; then federation_refuse "ambiguous_credential"; return; fi
  if [[ -n "$keychain" ]]; then
    token="$(security find-generic-password -a "${USER:-$(id -un)}" -s "$keychain" -w 2>/dev/null || true)"
    [[ -n "$token" ]] || { federation_refuse "credential_unavailable"; return; }
    federation="on (keychain)"
  else
    federation="on (file)"
  fi
  if [[ -z "$token" || "$token" =~ [[:space:]] ]]; then federation_refuse "credential is invalid"; return; fi
  export OCEAN_FEDERATION_URL="$url" OCEAN_FEDERATION_OWNER_TOKEN="$token"
}
# Presence selects the process source, including an explicitly empty value.
# The native daemon will reject an empty or partial pair; the launcher must not
# silently replace that explicit choice with ambient disk credentials first.
if [[ -n "${OCEAN_FEDERATION_URL+x}" || -n "${OCEAN_FEDERATION_OWNER_TOKEN+x}" ]]; then
  federation="on (process)"
elif [[ -e "$OCEAN_FEDERATION_ENV_FILE" ]]; then
  federation_load "$OCEAN_FEDERATION_ENV_FILE"
fi
echo "==> ocean-daemon: cwd=$NEUTRAL_CWD (neutral) bin=$BIN yolo=$OCEAN_YOLO bind=${OCEAN_BIND:-127.0.0.1:4780} federation=$federation"
cd "$NEUTRAL_CWD"
exec "$BIN"
