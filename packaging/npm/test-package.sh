#!/usr/bin/env bash
# Exercise the staged npm wrapper without publishing it.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "${1:-$SCRIPT_DIR}" && pwd)"
SOURCE_ROOT="$(cd "${SOURCE_ROOT:-$SCRIPT_DIR/../..}" && pwd)"
EXPECTED_VERSION="${EXPECTED_VERSION:-0.0.0-dev}"

command -v node >/dev/null 2>&1 || {
  echo "error: node is required" >&2
  exit 1
}
command -v npm >/dev/null 2>&1 || {
  echo "error: npm is required" >&2
  exit 1
}

for binary in ocean ocean-daemon; do
  path="$PKG_DIR/bin/$binary"
  if [[ ! -x "$path" ]]; then
    echo "error: staged binary is missing or not executable: $path" >&2
    exit 1
  fi
done

node -e '
  const fs = require("fs");
  const pkg = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
  const expectedVersion = process.argv[2];
  const expectedBins = {
    ocean: "bin/ocean",
    "ocean-daemon": "bin/ocean-daemon",
    "ocean-update": "ocean-update.sh",
  };
  const expectedFiles = [
    "bin/",
    "ocean-update.sh",
    "README.md",
    "LICENSE",
    "LICENSE-APACHE",
    "LICENSE-MIT",
    "NOTICE.md",
    "CREDITS.md",
    "TRADEMARKS.md",
    "THIRD-PARTY-LICENSES.txt",
  ];
  if (pkg.name !== "@risingtides-dev/ocean") throw new Error(`unexpected package name: ${pkg.name}`);
  if (pkg.version !== expectedVersion) throw new Error(`expected version ${expectedVersion}, got ${pkg.version}`);
  if (pkg.license !== "MIT OR Apache-2.0") throw new Error(`unexpected package license: ${pkg.license}`);
  if (JSON.stringify(pkg.bin) !== JSON.stringify(expectedBins)) throw new Error("unexpected bin map");
  if (JSON.stringify(pkg.files) !== JSON.stringify(expectedFiles)) throw new Error("unexpected package files allowlist");
  if (JSON.stringify(pkg.os) !== JSON.stringify(["darwin"])) throw new Error("package must be darwin-only");
  if (JSON.stringify(pkg.cpu) !== JSON.stringify(["arm64"])) throw new Error("package must be arm64-only");
  if (pkg.scripts) throw new Error("package must not define lifecycle scripts");
  if (pkg.dependencies || pkg.optionalDependencies || pkg.peerDependencies) {
    throw new Error("package wrapper must remain dependency-free");
  }
' "$PKG_DIR/package.json" "$EXPECTED_VERSION"

legal_files=(LICENSE LICENSE-APACHE LICENSE-MIT NOTICE.md CREDITS.md TRADEMARKS.md)
for legal_file in "${legal_files[@]}"; do
  if [[ ! -f "$PKG_DIR/$legal_file" ]]; then
    echo "error: staged package is missing $legal_file" >&2
    exit 1
  fi
  cmp "$SOURCE_ROOT/$legal_file" "$PKG_DIR/$legal_file"
done

inventory="$PKG_DIR/THIRD-PARTY-LICENSES.txt"
test -s "$inventory"
grep -q "cargo-about 0.9.1" "$inventory"
grep -q "BINARY: ocean (workspace package ocean-tui)" "$inventory"
grep -q "BINARY: ocean-daemon (workspace package ocean-daemon)" "$inventory"
grep -q -- "- ocean-tui " "$inventory"
grep -q -- "- ocean-daemon " "$inventory"
grep -q "UPSTREAM NOTICE FILES" "$inventory"
grep -q "moka 0.12.15 — NOTICE" "$inventory"
grep -q "src/common/frequency_sketch.rs" "$inventory"
grep -q "src/common/timer_wheel.rs" "$inventory"
grep -q "livekit-protocol 0.5.2 — protocol/NOTICE" "$inventory"
grep -q "livekit-protocol 0.7.8 — protocol/NOTICE" "$inventory"
grep -q "Copyright 2023 LiveKit, Inc." "$inventory"

sh -n "$PKG_DIR/ocean-update.sh"

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/ocean-npm-smoke.XXXXXX")"
trap 'rm -rf "$tmp_dir"' EXIT

manager_dir="$tmp_dir/managers"
mkdir -p "$manager_dir"
cat > "$manager_dir/bun" <<'FAKE'
#!/bin/sh
printf '%s\n' "$@" > "$OCEAN_UPDATE_ARGS"
FAKE
chmod +x "$manager_dir/bun"
OCEAN_UPDATE_ARGS="$tmp_dir/bun.args" PATH="$manager_dir:/usr/bin:/bin" \
  "$PKG_DIR/ocean-update.sh" > "$tmp_dir/bun.out"
test "$(cat "$tmp_dir/bun.args")" = "$(printf 'add\n-g\n@risingtides-dev/ocean@latest')"
grep -q "not hot-swapped" "$tmp_dir/bun.out"

rm "$manager_dir/bun"
cat > "$manager_dir/npm" <<'FAKE'
#!/bin/sh
printf '%s\n' "$@" > "$OCEAN_UPDATE_ARGS"
FAKE
chmod +x "$manager_dir/npm"
OCEAN_UPDATE_ARGS="$tmp_dir/npm.args" PATH="$manager_dir:/usr/bin:/bin" \
  "$PKG_DIR/ocean-update.sh" > "$tmp_dir/npm.out"
test "$(cat "$tmp_dir/npm.args")" = "$(printf 'install\n-g\n@risingtides-dev/ocean@latest')"
grep -q "not hot-swapped" "$tmp_dir/npm.out"

pack_json="$({
  cd "$PKG_DIR"
  npm pack --ignore-scripts --json --pack-destination "$tmp_dir"
})"
archive="$(printf '%s' "$pack_json" | node -e '
  let input = "";
  process.stdin.setEncoding("utf8");
  process.stdin.on("data", chunk => input += chunk);
  process.stdin.on("end", () => {
    const result = JSON.parse(input);
    if (!Array.isArray(result) || result.length !== 1 || !result[0].filename) {
      throw new Error("npm pack returned an unexpected result");
    }
    process.stdout.write(result[0].filename);
  });
')"
archive_path="$tmp_dir/$archive"
[[ -f "$archive_path" ]]

actual_files="$(tar -tzf "$archive_path" | sed '/\/$/d' | LC_ALL=C sort)"
expected_files="$(cat <<'FILES' | LC_ALL=C sort
package/CREDITS.md
package/LICENSE
package/LICENSE-APACHE
package/LICENSE-MIT
package/NOTICE.md
package/README.md
package/THIRD-PARTY-LICENSES.txt
package/TRADEMARKS.md
package/bin/ocean
package/bin/ocean-daemon
package/ocean-update.sh
package/package.json
FILES
)"
if [[ "$actual_files" != "$expected_files" ]]; then
  echo "error: packed file set differs from the twelve-file contract" >&2
  diff -u <(printf '%s\n' "$expected_files") <(printf '%s\n' "$actual_files") || true
  exit 1
fi

mkdir -p "$tmp_dir/unpacked"
tar -xzf "$archive_path" -C "$tmp_dir/unpacked"
for legal_file in "${legal_files[@]}"; do
  cmp "$SOURCE_ROOT/$legal_file" "$tmp_dir/unpacked/package/$legal_file"
done
cmp "$inventory" "$tmp_dir/unpacked/package/THIRD-PARTY-LICENSES.txt"

prefix="$tmp_dir/prefix"
npm install --global --ignore-scripts --no-audit --no-fund --prefix "$prefix" "$archive_path" >/dev/null
for command_name in ocean ocean-daemon ocean-update; do
  if [[ ! -x "$prefix/bin/$command_name" ]]; then
    echo "error: npm install did not expose executable $command_name" >&2
    exit 1
  fi
done

ocean_target="$(readlink "$prefix/bin/ocean")"
daemon_target="$(readlink "$prefix/bin/ocean-daemon")"
if [[ "$(dirname "$ocean_target")" != "$(dirname "$daemon_target")" ]]; then
  echo "error: ocean and ocean-daemon did not install as sibling binaries" >&2
  exit 1
fi

if [[ "${RUN_BUN_SMOKE:-0}" == "1" ]]; then
  command -v bun >/dev/null 2>&1 || {
    echo "error: RUN_BUN_SMOKE=1 but bun is unavailable" >&2
    exit 1
  }
  bun_root="$tmp_dir/bun"
  mkdir -p "$tmp_dir/bun-home"
  HOME="$tmp_dir/bun-home" BUN_INSTALL="$bun_root" \
    bun add --global "$archive_path" --cache-dir "$tmp_dir/bun-cache" --no-progress >/dev/null
  for command_name in ocean ocean-daemon ocean-update; do
    if [[ ! -x "$bun_root/bin/$command_name" ]]; then
      echo "error: bun install did not expose executable $command_name" >&2
      exit 1
    fi
  done
fi

printf 'package smoke: PASS (%s, version %s, bun=%s)\n' \
  "$archive" "$EXPECTED_VERSION" "${RUN_BUN_SMOKE:-0}"
