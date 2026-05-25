# ocean-rs agent instructions

This repo is a Rust-native Ocean OS agent harness.

Principles:
- Do not port Pi TypeScript line-by-line.
- Recreate the concepts in idiomatic Rust.
- Keep daemon/client protocol stable and simple.
- Prefer small crates with clear boundaries.
- Keep one-shot paths fast.
- Run `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and tests before finalizing.
