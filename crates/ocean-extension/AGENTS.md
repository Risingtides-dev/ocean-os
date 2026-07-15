# ocean-extension — Package Schema Boundary

## Purpose

Own schema-v1 `ocean-extension.toml` parsing, SemVer compatibility, and canonical package-resource validation without executing package code.

## Ownership

- **Scope:** `crates/ocean-extension/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Does not own:** inner `plugin.toml`/stdio behavior, install/trust/enable state, daemon routes, lifecycle, or service execution

## Local Contracts

- Keep raw parsed manifests distinct from validated canonical resources.
- Schema v1 fails closed on unknown fields and resource kinds.
- Validation is filesystem inspection only: it must never launch package code.
- Every declared filesystem resource canonicalizes beneath the canonical package root.
- Manifests contain capability names/references, never secret or environment values. Schema-v1 secret references use the host-resolvable `<scheme>:<key>` grammar documented by `SecretReference`; syntax validation does not prove publisher intent.

## Work Guidance

- Keep this a dependency-light leaf crate using only `serde`, `toml`, `semver`, and `std`.
- Preserve MSRV 1.88 and return typed `Result` errors for all untrusted input.

## Verification

- `cargo test -p ocean-extension`
- `cargo clippy -p ocean-extension --all-targets -- -D warnings`

## Child devlog Index

No child boundaries defined.
