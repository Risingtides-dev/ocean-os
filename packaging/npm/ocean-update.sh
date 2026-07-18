#!/bin/sh
# ocean-update — pull the latest published Ocean build in one pass.
set -eu

PKG="@risingtides-dev/ocean"

if command -v bun >/dev/null 2>&1; then
  echo "Updating $PKG via bun..."
  bun add -g "$PKG@latest"
elif command -v npm >/dev/null 2>&1; then
  echo "Updating $PKG via npm..."
  npm install -g "$PKG@latest"
else
  echo "error: neither bun nor npm found on PATH" >&2
  exit 1
fi

cat <<'NOTICE'
Ocean package updated.
- A launchd-supervised daemon remains installer-owned and was not changed.
- An already-running unsupervised daemon is not hot-swapped. Finish active
  turns and restart that exact daemon process before relying on the new build.
NOTICE
