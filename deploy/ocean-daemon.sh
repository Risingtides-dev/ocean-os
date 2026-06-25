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
# Mirrors what the daemon was hand-launched as before this ticket:
#     cd <repo> && OCEAN_YOLO=1 ./target/release/ocean-daemon
# i.e. cwd = repo root, env = OCEAN_YOLO=1, default bind 127.0.0.1:4780.
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

# Production env. Mirrors the prior hand-launch exactly; override via the plist's
# EnvironmentVariables block or by exporting before launch.
#   OCEAN_YOLO=1            -> operator default: tools run without per-call gating.
#   OCEAN_BIND (optional)   -> defaults to 127.0.0.1:4780 inside the binary.
#   OCEAN_ASSISTANTS_DIR    -> optional; defaults to ~/.config/ocean-rs/assistants.
export OCEAN_YOLO="${OCEAN_YOLO:-1}"

echo "==> ocean-daemon: cwd=$REPO yolo=$OCEAN_YOLO bind=${OCEAN_BIND:-127.0.0.1:4780}"
cd "$REPO"
exec "$BIN"
