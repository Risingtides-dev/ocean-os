# ocean-rs agent instructions

This repo is a Rust-native Ocean OS agent harness.

Principles:
- Longhouse is the hive: the local-first agentic operations hub where agents go before they act. See `docs/LONGHOUSE.md`.
- `ocean-daemon` owns local sessions, streaming, filesystem/tools, permission gates, and execution authority.
- `ocean-longhouse` owns SOPs, routines/workflows, tools/MCP discovery, skills, memory/knowledge, subagent specs, and quorum/council coordination.
- Do not port Pi TypeScript line-by-line.
- Recreate the concepts in idiomatic Rust.
- Keep daemon/client protocol stable and simple.
- Prefer small crates with clear boundaries.
- Keep one-shot paths fast.
- Run `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and tests before finalizing.
