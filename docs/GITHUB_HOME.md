# GitHub Home

Ocean's canonical GitHub home is:

```text
https://github.com/Risingtides-dev/ocean-os
```

## Repository Meaning

`Risingtides-dev/ocean-os` is the public umbrella repository for Ocean:

```text
ocean-rs      canonical Rust runtime daemon / local agent node
ocean-tui     terminal steering client
ocean-native  native GUI client integration path
distro        service/supervisor/OS integration over time
```

## Current Local Sources

Primary runtime source on tide-net:

```text
/home/smathdaddy/code/rust/ocean-rs
```

Current GUI workspace:

```text
/home/ocean-os
```

The GUI workspace is being migrated into a thin daemon client. The runtime authority remains `ocean-rs`.

## Remote Setup

Both local workspaces currently point at:

```bash
git remote set-url origin https://github.com/Risingtides-dev/ocean-os.git
```

GitHub CLI authentication was not available on tide-net when this file was written, so pushing/creating the remote requires one of:

```bash
gh auth login
```

or a configured `GH_TOKEN`.

## Publish Rule

Before pushing:

```bash
cd /home/smathdaddy/code/rust/ocean-rs
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
```

Do not commit local Pi messenger state, secrets, `.env`, `target/`, or gateway inbox files.
