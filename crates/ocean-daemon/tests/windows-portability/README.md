# Windows registry portability harness

This isolated, repository-owned package cross-compiles the production
`src/extension_registry.rs` and `src/extension_service_unsupported.rs` modules
for Windows. It does not copy either implementation. The
`registry-portability-check` feature removes only the registry's daemon
`AppState`/Axum route coupling; reader validation and production platform cfgs
remain unchanged.

Run from the repository root:

```sh
cargo zigbuild \
  --manifest-path crates/ocean-daemon/tests/windows-portability/Cargo.toml \
  --features registry-portability-check \
  --target x86_64-pc-windows-gnu
```

The binary references the actual unsupported supervisor start/shutdown path,
which invokes the actual coherent common reader. On the Windows target this
compiles the `NtCreateFile`/`NtQueryDirectoryFile` descriptor-relative,
reparse-safe path and the supervisor that exposes only `unsupported_platform`
status without any child-process API.

This cross-build is compile evidence, not Windows runtime evidence. A full
`ocean-daemon` Windows cross-build is separately blocked by pre-existing
Unix-only code outside Stage A2a, currently including
`crates/ocean-observatory/src/auth.rs` Unix permission imports and flags. Those
full-daemon blockers are not bypassed or weakened by this harness.
