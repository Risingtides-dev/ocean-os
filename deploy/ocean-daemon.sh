#!/usr/bin/env bash
# Supervised launcher for the Ocean daemon (OCEAN-253).
#
# This is the entrypoint launchd execs (via dev.risingtides.ocean-daemon.plist).
# It exec's the PREBUILT release binary with the production env. A supervised
# service must respawn fast and deterministically, so this script does NOT run
# `cargo build` — the binary is built once at install time (see
# ops/install-ocean-daemon.sh) from MAIN, per the operator's build-from-main
# rule. To pick up new code: rebuild from main, then kickstart -k (see ops/README).
#
# The daemon refuses to start from inside a git repo so unbound turns cannot
# accidentally bind to the daemon source checkout. Keep the binary path pinned
# to this repo, but run it from a neutral cwd.
set -euo pipefail

# Repo root = parent of this deploy/ dir, resolved absolutely (symlink-safe).
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Toolchain + common bins on PATH (launchd starts with a minimal PATH). The
# daemon shells out to tools (git, ripgrep, etc.) for its own tool calls, so a
# sane PATH matters even though this script doesn't compile anything.
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin:$PATH"

BIN="$REPO/target/release/ocean-daemon"
if [[ ! -x "$BIN" ]]; then
  echo "FATAL: $BIN not found or not executable." >&2
  echo "       Build it first (from MAIN):  cargo build -p ocean-daemon --release" >&2
  echo "       (ops/install-ocean-daemon.sh does this for you.)" >&2
  exit 127
fi

# Production env. Override via the plist's EnvironmentVariables block or by
# exporting before launch.
#   OCEAN_YOLO=1            -> operator default: tools run without per-call gating.
#   OCEAN_BIND (optional)   -> defaults to 127.0.0.1:4780 inside the binary.
#   OCEAN_ASSISTANTS_DIR    -> defaults to sibling ocean-agents/assistants when present.
export OCEAN_YOLO="${OCEAN_YOLO:-1}"
if [[ -z "${OCEAN_ASSISTANTS_DIR:-}" && -d "$REPO/../ocean-agents/assistants" ]]; then
  export OCEAN_ASSISTANTS_DIR="$(cd "$REPO/../ocean-agents/assistants" && pwd)"
fi

RUN_CWD="${OCEAN_DAEMON_RUN_CWD:-$HOME}"
echo "==> ocean-daemon: cwd=$RUN_CWD bin=$BIN yolo=$OCEAN_YOLO bind=${OCEAN_BIND:-127.0.0.1:4780} assistants=${OCEAN_ASSISTANTS_DIR:-default}"
cd "$RUN_CWD"
exec "$BIN"
