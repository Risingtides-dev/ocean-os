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
# cwd is NEUTRAL ($HOME), not the repo. The daemon is workspace-agnostic
# (turns carry their own cwd); its startup guard refuses to boot from inside a
# git repo so unbound fallback turns don't bind to ocean-os (see main.rs,
# OCEAN_ALLOW_REPO_CWD). Pre-guard this mirrored the hand-launch
# (cd <repo> && OCEAN_YOLO=1 ./target/release/ocean-daemon); the guard made that
# cwd invalid, so we run from $HOME instead. BIN is resolved absolutely, so
# repo-cwd isn't needed to find the binary. Override via OCEAN_DAEMON_CWD.
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

# Run from a NEUTRAL cwd so the startup guard's repo-cwd check passes and the
# unbound-turn fallback anchor is harmless (home, not ocean-os).
NEUTRAL_CWD="${OCEAN_DAEMON_CWD:-$HOME}"
echo "==> ocean-daemon: cwd=$NEUTRAL_CWD (neutral) bin=$BIN yolo=$OCEAN_YOLO bind=${OCEAN_BIND:-127.0.0.1:4780}"
cd "$NEUTRAL_CWD"
exec "$BIN"
