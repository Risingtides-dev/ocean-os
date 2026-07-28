# Windows registry portability harness

## Purpose

Cross-compile the actual daemon extension registry reader and unsupported
supervisor source modules for Windows without pulling in unrelated full-daemon
platform blockers.

## Ownership

- Scope: this isolated nested Cargo package.
- Parent contracts: `../../AGENTS.md`, `../../../AGENTS.md`, and
  `../../../../AGENTS.md`.
- Production source remains owned by `../../src/extension_registry.rs` and
  `../../src/extension_service_unsupported.rs`; never copy it here.

## Local Contracts

- Keep this package outside the root Cargo workspace via its local `[workspace]`.
- Use `#[path]` to compile the actual production modules.
- `registry-portability-check` may remove only daemon AppState/Axum route
  coupling. Do not weaken platform cfgs, reparse validation, or reader authority.
- The harness must reference the real unsupported supervisor start/shutdown
  path and must not add a child-process dependency.

## Work Guidance

- Document full-daemon Windows blockers separately from harness results.

## Verification

- `cargo zigbuild --manifest-path crates/ocean-daemon/tests/windows-portability/Cargo.toml --features registry-portability-check --target x86_64-pc-windows-gnu`

## Child devlog Index

No child boundaries defined.
