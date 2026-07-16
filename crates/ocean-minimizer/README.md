# ocean-minimizer

Standalone, conservative command-output minimization for Ocean OS. M1 is a
library checkpoint only: it is not wired to the daemon, runtime, agent, TUI,
harness profiles, or artifact storage.

## API

Callers construct `Invocation { program, args }` from an argv they have already
tokenized, then call `minimize(&invocation, capture, exit_code)` (or
`invocation.minimize(...)`). The result includes:

- deterministic `Disposition` and typed `PassthroughReason`;
- exact input/output UTF-8 byte and logical-line `Accounting`;
- `original_text` only when output changed.

M1 recognizes conservative human output for:

- `cargo build`, `check`, `test`, `clippy`, and `fmt`;
- default human `git status` and `git log`;
- default `gh pr checks` tables;
- `npm install`/`i`/`ci` progress noise;
- the `npx` first-install preamble;
- pytest summaries and failure/error blocks.

Unknown commands, unrecognized human shapes, NUL output, and explicit
porcelain/JSON/custom-format/diff/watch/log modes pass through byte-for-byte.
Captures over 4 MiB fail open. Changed output has one final fixed 200-line cap.
There is no shell parser, regular expression engine, TOML or user
configuration, artifact persistence/reference, or appended footer.

## Validation

```bash
cargo test -p ocean-minimizer
cargo clippy -p ocean-minimizer --all-targets -- -D warnings
```

## Provenance

The mechanism and selected fixtures are adapted from Oh My Pi at
`03c48d073bd4849726cc14750b5aecfa310bdf26`. Pytest state-machine concepts
carry RTK attribution pinned at
`878af7de99e0ba71da2e8fd996f6b52a1836e06c`. See `LICENSE` and `NOTICE`.
