#!/usr/bin/env bash
# Build and atomically install the reviewed Ocean TUI as ~/.local/bin/ocean.
#
# Feature branches MUST still be built and tested. This script is the separate
# production-install gate: it only publishes clean content already contained in
# origin/main, records its revision in the artifact name, and never overwrites a
# running Mach-O in place.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/usr/local/bin:/opt/homebrew/bin:$PATH"

git -C "$REPO" fetch origin main --quiet
branch="$(git -C "$REPO" branch --show-current)"
if ! git -C "$REPO" merge-base --is-ancestor HEAD origin/main; then
  echo "FATAL: $branch at $(git -C "$REPO" rev-parse --short=12 HEAD) is not contained in origin/main." >&2
  echo "       Build/test feature branches freely; merge the reviewed change before installing it as ocean." >&2
  exit 64
fi
if [[ -n "$(git -C "$REPO" status --porcelain)" ]]; then
  echo "FATAL: the checkout is not clean; the installed TUI must have exact provenance." >&2
  exit 64
fi

printf '==> building ocean-tui release from %s\n' "$(git -C "$REPO" rev-parse --short=12 HEAD)"
( cd "$REPO" && cargo build --locked -p ocean-tui --release )

src="$REPO/target/release/ocean-tui"
[[ -x "$src" ]] || { echo "FATAL: build did not produce $src" >&2; exit 1; }

rev="$(git -C "$REPO" rev-parse --short=12 HEAD)"
libexec="$HOME/.local/libexec/ocean-tui"
dest="$libexec/ocean-$rev"
mkdir -p "$libexec" "$HOME/.local/bin"
candidate="$libexec/.ocean-$rev.$$"
cleanup() {
  rm -f "$candidate"
  [[ -z "${current_tmp:-}" ]] || rm -f "$current_tmp"
  [[ -z "${bin_tmp:-}" ]] || rm -f "$bin_tmp"
}
trap cleanup EXIT
install -m 0755 "$src" "$candidate"
codesign --force --sign - "$candidate"
codesign --verify --deep --strict "$candidate"
if [[ -e "$dest" ]]; then
  if ! cmp -s "$candidate" "$dest"; then
    echo "FATAL: immutable artifact already exists with different content: $dest" >&2
    exit 65
  fi
  rm -f "$candidate"
  codesign --verify --deep --strict "$dest"
else
  mv "$candidate" "$dest"
fi

current_tmp="$libexec/.current.$$"
ln -s "$dest" "$current_tmp"
mv -f "$current_tmp" "$libexec/current"

bin_tmp="$HOME/.local/bin/.ocean.$$"
ln -s "$libexec/current" "$bin_tmp"
mv -f "$bin_tmp" "$HOME/.local/bin/ocean"

# Keep the three newest immutable TUI artifacts; current's target always remains.
ls -t "$libexec"/ocean-* 2>/dev/null | tail -n +4 | while read -r old; do
  [[ "$(readlink "$libexec/current")" == "$old" ]] || rm -f "$old"
done

printf '==> installed %s -> %s\n' "$HOME/.local/bin/ocean" "$dest"
printf '==> next: run a real multi-second PTY smoke; --help is not a launch proof\n'
trap - EXIT
