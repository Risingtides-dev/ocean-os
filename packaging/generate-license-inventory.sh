#!/usr/bin/env bash
# Generate the full-text dependency-license inventory shipped with both releases.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <cargo-about-bin> <output-file>" >&2
  exit 64
fi

cargo_about="$1"
output_file="$2"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
expected_version="cargo-about 0.9.1"

command -v python3 >/dev/null 2>&1 || {
  echo "error: python3 is required to collect dependency NOTICE files" >&2
  exit 1
}
if [[ ! -x "$cargo_about" ]]; then
  echo "error: cargo-about is missing or not executable: $cargo_about" >&2
  exit 1
fi
if [[ "$("$cargo_about" --version)" != "$expected_version" ]]; then
  echo "error: dependency inventory requires exactly $expected_version" >&2
  exit 1
fi

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/ocean-license-inventory.XXXXXX")"
trap 'rm -rf "$tmp_dir"' EXIT

render_graph() {
  local package="$1"
  local text_destination="$2"
  local json_destination="$3"
  local manifest="$repo_root/crates/$package/Cargo.toml"
  "$cargo_about" generate \
    --frozen \
    --fail \
    --manifest-path "$manifest" \
    --config "$script_dir/about.toml" \
    --output-file "$text_destination" \
    "$script_dir/about.hbs"
  "$cargo_about" generate \
    --frozen \
    --fail \
    --format json \
    --manifest-path "$manifest" \
    --config "$script_dir/about.toml" \
    --output-file "$json_destination"
  test -s "$text_destination"
  test -s "$json_destination"
  # cargo-about inventories third-party packages in the selected binary graph.
  # The workspace root package is publish=false and is intentionally omitted;
  # its project licenses are shipped separately beside this inventory.
}

render_graph ocean-tui "$tmp_dir/ocean-tui.txt" "$tmp_dir/ocean-tui.json"
render_graph ocean-daemon "$tmp_dir/ocean-daemon.txt" "$tmp_dir/ocean-daemon.json"

python3 - "$tmp_dir/ocean-tui.json" "$tmp_dir/ocean-daemon.json" > "$tmp_dir/upstream-notices.txt" <<'PY'
import json
import os
from pathlib import Path
import sys

notices = {}
for json_path in sys.argv[1:]:
    document = json.loads(Path(json_path).read_text(encoding="utf-8"))
    for crate in document["crates"]:
        package = crate["package"]
        root = Path(package["manifest_path"]).parent
        for directory, dirnames, filenames in os.walk(root, followlinks=False):
            dirnames[:] = sorted(
                name for name in dirnames if name not in {".git", "target"}
            )
            for filename in sorted(filenames):
                if filename.upper() not in {"NOTICE", "NOTICE.MD", "NOTICE.TXT"}:
                    continue
                path = Path(directory, filename)
                relative = path.relative_to(root).as_posix()
                key = (package["name"], package["version"], relative)
                text = path.read_text(encoding="utf-8").rstrip()
                previous = notices.setdefault(key, text)
                if previous != text:
                    raise RuntimeError(f"notice content changed while scanning {key}")

print("UPSTREAM NOTICE FILES")
print()
print("These notice files were discovered in the same locked binary dependency")
print("graphs as the license inventory above and are reproduced verbatim.")
for (name, version, relative), text in sorted(notices.items()):
    print()
    print("=" * 80)
    print(f"{name} {version} — {relative}")
    print()
    print(text)
PY

test -s "$tmp_dir/upstream-notices.txt"
grep -q "moka 0.12.15 — NOTICE" "$tmp_dir/upstream-notices.txt"
grep -q "src/common/frequency_sketch.rs" "$tmp_dir/upstream-notices.txt"
grep -q "src/common/timer_wheel.rs" "$tmp_dir/upstream-notices.txt"
grep -q "livekit-protocol 0.5.2 — protocol/NOTICE" "$tmp_dir/upstream-notices.txt"
grep -q "livekit-protocol 0.7.8 — protocol/NOTICE" "$tmp_dir/upstream-notices.txt"
grep -q "Copyright 2023 LiveKit, Inc." "$tmp_dir/upstream-notices.txt"

{
  cat <<'HEADER'
THIRD-PARTY SOFTWARE LICENSES

This inventory accompanies the Ocean macOS arm64 binary distribution. It was
generated from the locked Rust dependency graphs with cargo-about 0.9.1.
Development-only dependencies are excluded; normal, build, and transitive
dependencies for aarch64-apple-darwin are included. Applicable upstream NOTICE
files discovered in those graphs are reproduced verbatim at the end. This
inventory supplements the project LICENSE and NOTICE files shipped beside it.

================================================================================
BINARY: ocean (workspace package ocean-tui)
================================================================================
HEADER
  cat "$tmp_dir/ocean-tui.txt"
  cat <<'HEADER'

================================================================================
BINARY: ocean-daemon (workspace package ocean-daemon)
================================================================================
HEADER
  cat "$tmp_dir/ocean-daemon.txt"
  printf '\n'
  cat "$tmp_dir/upstream-notices.txt"
  printf '\n'
} > "$tmp_dir/THIRD-PARTY-LICENSES.txt"

mkdir -p "$(dirname "$output_file")"
mv "$tmp_dir/THIRD-PARTY-LICENSES.txt" "$output_file"
