# @risingtides-dev/ocean

Prebuilt Ocean binaries (`ocean` TUI + `ocean-daemon`) for macOS arm64,
published to GitHub Packages by the tag-triggered release workflow.

## One-time setup (each teammate)

1. Install [bun](https://bun.com) and the GitHub CLI, then log in:

   ```sh
   brew install oven-sh/bun/bun gh
   gh auth login
   gh auth refresh -s read:packages
   ```

2. Point the `@risingtides-dev` scope at GitHub Packages:

   ```sh
   printf '%s\n' \
     "@risingtides-dev:registry=https://npm.pkg.github.com" \
     "//npm.pkg.github.com/:_authToken=$(gh auth token)" >> ~/.npmrc
   ```

3. Install:

   ```sh
   bun add -g @risingtides-dev/ocean
   ```

This puts `ocean`, `ocean-daemon`, and `ocean-update` on PATH. The TUI
discovers `ocean-daemon` as a sibling binary automatically.

## Updating

```sh
ocean-update
```

That reinstalls the package-local `ocean` + `ocean-daemon` pair via bun (or
npm as fallback). It does not replace an already-running daemon process. Finish
active turns and restart the exact unsupervised `ocean-daemon` process before
expecting a newly installed sibling binary to serve requests; `ocean-update`
prints this reminder instead of guessing at or pattern-killing a process.

### Existing supervised daemon installations

The npm package does not rewrite launchd state or
`~/.local/libexec/ocean-daemon/current`. If the
`dev.risingtides.ocean-daemon` LaunchAgent is installed, the TUI intentionally
uses that separately managed immutable daemon artifact rather than directly
spawning the package sibling. Update that supervised daemon from an updated
`main` checkout with `ops/install-ocean-daemon.sh`; `ocean-update` updates only
the npm distribution. On machines without the LaunchAgent, the TUI discovers
and directly starts the package's sibling `ocean-daemon`.

## Publishing a release (maintainers)

Only stable `vMAJOR.MINOR.PATCH` tags are accepted, and the tagged commit must
already be contained in `origin/main`. The repository must also keep the active,
no-bypass **Ocean immutable release tags** tag ruleset (pinned repository
ruleset id `19331797`) for `refs/tags/v*`, with updates, deletion, and
non-fast-forward changes blocked. The workflow verifies
that policy before publishing and fails closed if it is absent or weakened:

```sh
git switch main
git pull --ff-only
git tag v0.2.0
git push origin v0.2.0
```

`.github/workflows/release.yml` also validates packaging changes on pull
requests. It reads the exact Rust version from the repository's
`rust-toolchain.toml`, builds `ocean-tui` + `ocean-daemon` with `--locked` on a
verified macOS arm64 runner, stages them into this package as `bin/ocean` and
`bin/ocean-daemon`, and derives the package version from the tag. It downloads
the official cargo-about 0.9.1 arm64 asset only after matching the repository-
pinned SHA-256, then generates the TUI and daemon dependency-license graphs
with `--frozen --fail`, reproduces applicable Moka and LiveKit protocol NOTICE
files from those exact graphs, and requires byte-identical consecutive output.

The npm package uses the SPDX expression `MIT OR Apache-2.0`. Its exact
12-file payload is the two binaries, updater, README, package metadata, root
`LICENSE`, `LICENSE-APACHE`, `LICENSE-MIT`, `NOTICE.md`, `CREDITS.md`,
`TRADEMARKS.md`, and generated `THIRD-PARTY-LICENSES.txt`. `test-package.sh`
byte-compares the project legal files to the current source tree, checks the
full-text license and upstream-NOTICE inventory, packs the wrapper, and installs it under temporary npm and
Bun prefixes to verify executable links, dependency-free metadata, and sibling
binary layout. The GitHub `ocean-macos-arm64.tar.gz` carries the same legal and
inventory files beside `ocean` and `ocean-daemon`, for an exact nine-file
binary-archive payload.

The read-only validation job uploads one exact artifact. A separate
write-permitted job downloads its ZIP by immutable artifact id, verifies the
upload SHA-256 before extraction, verifies the immutable-tag ruleset, and
re-peels the live tag to the event commit immediately before both release and
package mutation. It then attaches the verified binary archive plus its
checksum to the GitHub Release and publishes the npm package. Reruns accept an
existing version only when its npm integrity matches the validated payload.
Every successfully completing publish converges `latest` to the greatest stable
registry version and confirms that state twice, so concurrent or older tag jobs
cannot leave teammate updates pointing backward. GitHub Release assets remain
tag-addressed and deliberately do not mutate the repository-wide **Latest
Release** pointer, whose value would otherwise depend on job completion order.

`bin/` is git-ignored: binaries exist only in CI staging and in the published
tarball, never in the repo.

## Auth and package-access notes

- `gh auth token` tokens can expire/rotate. If installs start failing with
  401s, rerun step 2 to refresh `~/.npmrc`.
- GitHub's npm registry does not infer public package visibility merely because
  the source repository is public. Publishing with this repository's
  `GITHUB_TOKEN` links repository access, but after the first publish an owner
  must verify the intended visibility and inherited repository access in the
  package settings before giving teammates the install instructions.
