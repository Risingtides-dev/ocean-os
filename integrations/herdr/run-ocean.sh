#!/bin/sh
set -eu

if [ -n "${OCEAN_BIN:-}" ]; then
    ocean_bin=$OCEAN_BIN
elif command -v ocean >/dev/null 2>&1; then
    ocean_bin=$(command -v ocean)
elif command -v ocean-tui >/dev/null 2>&1; then
    ocean_bin=$(command -v ocean-tui)
else
    printf '%s\n' 'Ocean binary not found. Install `ocean`/`ocean-tui` or set OCEAN_BIN.' >&2
    exit 127
fi

exec "$ocean_bin" --project "$PWD"
