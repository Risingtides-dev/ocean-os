# Packaging — Team Distribution

## Purpose

Distribution artifacts for shipping prebuilt Ocean binaries to teammates.

## Ownership

- `npm/` — the `@risingtides-dev/ocean` npm wrapper package: prebuilt
  `ocean` (TUI) + `ocean-daemon` binaries for macOS arm64, published to
  GitHub Packages by `.github/workflows/release.yml` on `v*` tag push.
- `about.toml`, `about.hbs`, and `generate-license-inventory.sh` — pinned,
  fail-closed generation of the full-text dependency-license inventory shipped
  in both release formats.

## Local Contracts

- `npm/bin/` is git-ignored; binaries are staged only in CI and shipped
  only in the published tarball. Never commit binaries.
- The package version comes from a stable `vMAJOR.MINOR.PATCH` release tag
  (`v0.2.0` → `0.2.0`); the committed `package.json` version stays a
  `0.0.0-dev` placeholder. Release tags must already be contained in
  `origin/main`; prerelease and branch-only tags fail closed.
- Package scope must remain `@risingtides-dev` (GitHub Packages requires
  the scope to match the repository owner).
- The `bin` map must keep `ocean` and `ocean-daemon` in the same install
  directory: on machines without launchd supervision, the TUI discovers the
  daemon as a sibling of its own binary
  (`crates/ocean-tui/src/shell/daemon_boot.rs`).
- The npm wrapper does not own launchd or
  `~/.local/libexec/ocean-daemon/current`. An installed
  `dev.risingtides.ocean-daemon` LaunchAgent continues to use the immutable
  artifact published by `ops/install-ocean-daemon.sh`; package updates must not
  silently flip that separately supervised artifact.
- The release workflow must consume the exact `rust-toolchain.toml` pin, build
  with the lockfile, and fail closed unless both the runner and built binaries
  are macOS arm64.
- The npm package declares `MIT OR Apache-2.0`. Both the 12-file npm payload and
  nine-file GitHub binary archive carry byte-identical root `LICENSE`,
  `LICENSE-APACHE`, `LICENSE-MIT`, `NOTICE.md`, `CREDITS.md`, and
  `TRADEMARKS.md`, plus generated `THIRD-PARTY-LICENSES.txt` containing full
  license texts and applicable upstream NOTICE files; do not restore the
  pre-license five-file/binary-only payloads.
- The dependency inventory is generated separately for the `ocean-tui` and
  `ocean-daemon` macOS arm64 normal/build/transitive graphs, excluding dev-only
  dependencies. Cargo-about lists third-party graph packages; the workspace
  roots are `publish=false` and intentionally absent because the six project
  legal files ship separately beside the generated inventory. The validation job first runs `cargo fetch --locked` so a clean
  runner hydrates every locked source Cargo metadata may require; inventory
  generation itself remains frozen and offline. Generation uses cargo-about
  0.9.1 from the official arm64 asset pinned to SHA-256
  `6a38fe166d17a674269d4373256c0b6bd93acc2553e12de0517cb9ecc73c9c02`,
  runs `--frozen --fail`, reproduces the graph's Moka and LiveKit protocol
  NOTICE files, and must produce byte-identical consecutive outputs.
- Every external GitHub Action in the release lane is pinned to a reviewed full
  commit SHA. Validation runs read-only with checkout credentials removed;
  release/package write permissions exist only in the no-checkout publish job.
  That job downloads the artifact ZIP by immutable id and compares it to the
  upload SHA-256 before extraction. Publication also requires the active,
  no-bypass `Ocean immutable release tags` repository ruleset (pinned id
  `19331797`) for `refs/tags/v*` with update, deletion, and non-fast-forward
  protection; the workflow verifies the policy and re-peels the live tag to
  the event commit before both GitHub Release and npm mutations.
- Publication is retry-safe: an existing npm version is accepted only when its
  integrity equals the validated artifact. Every completing publish converges
  npm `latest` to the greatest stable registry version with two observations;
  GitHub Releases stay tag-addressed and must not mutate the repository-wide
  Latest Release pointer based on completion order.
- Teammate setup and update flow is documented in `npm/README.md`; keep it
  accurate when the workflow or package layout changes, including package
  access settings and the non-hot-swapped unsupervised daemon caveat.

## Work Guidance

- Keep the wrapper dependency-free: no install scripts, no runtime JS.
- Cut releases by tagging `main`: `git tag vX.Y.Z && git push origin vX.Y.Z`.

## Verification

- `node -e "JSON.parse(require('fs').readFileSync('packaging/npm/package.json','utf8'))"`
- `bash -n packaging/generate-license-inventory.sh packaging/npm/test-package.sh`
- `sh -n packaging/npm/ocean-update.sh`
- With the checksum-verified cargo-about 0.9.1 binary, generate twice and
  `cmp` the results:
  `packaging/generate-license-inventory.sh /path/to/cargo-about /tmp/licenses.txt`.
- With executable binaries plus the six project legal files and generated
  inventory assembled as the workflow's 12-file staging package:
  `RUN_BUN_SMOKE=1 EXPECTED_VERSION=0.0.0-dev packaging/npm/test-package.sh /path/to/staged/npm`.
- After a tag push: confirm the Release workflow is green, verify the release
  tarball checksum, and confirm
  `bun add -g @risingtides-dev/ocean@latest` resolves the new version.

## Child devlog Index
