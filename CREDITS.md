# Credits and acknowledgements

Ocean OS stands on generous open-source work. This page recognizes the people
and projects whose ideas or implementations materially shaped Ocean; the legal
terms and pinned donor details are in [`NOTICE.md`](NOTICE.md).

## Pi

Thank you to **Mario Zechner** and the Pi contributors for building the Pi
agent harness and its direct, extensible approach to coding agents.

`ocean-runtime` and `ocean-protocol` began from the MIT-licensed `pi-agent` and
`pi-ai` Rust crates, version 1.0.0. Those crates describe themselves as Rust
ports of the Pi agent core and provider layer and were published to crates.io
by **Naoki Takata**. Ocean has since changed their architecture substantially,
but that starting point deserves explicit credit.

- Pi: <https://github.com/earendil-works/pi>
- Rust crates: <https://crates.io/crates/pi-agent> and
  <https://crates.io/crates/pi-ai>

## Oh My Pi

Thank you to **Can Bölük**, **Mario Zechner**, and the Oh My Pi contributors.
Ocean contains ports, reimplementations, adapted tests, or source-informed
mechanisms from Oh My Pi across hashline editing, code intelligence, structural
summaries, OAuth/provider behavior, filesystem walking and search, command
output minimization, retry/tool scheduling, and selected terminal interaction
patterns.

Ocean keeps these mechanisms within its own daemon authority, permission
model, protocol, and Rust architecture; it does not import OMP's package or
orchestration boundaries wholesale.

- Oh My Pi: <https://github.com/can1357/oh-my-pi>
- Principal audited revision: `03c48d073bd4849726cc14750b5aecfa310bdf26`

## inertia-tui and the 3D project graph

Thank you to **Kairav Mittal** (`aclfe`) for
[`inertia-tui`](https://github.com/aclfe/inertia). Ocean's terminal project
graph reimplements and adapts inertia-tui's 3D camera basis transform,
perspective projection, orbit/pan/zoom controls, and 2:1 terminal-cell aspect
correction. Ocean uses local graph-specific data structures and rendering, but
the mathematical donor is named directly in the source and notices.

- Audited donor revision: `99196825d5f62bd7524485c411f4b1e58d4f8a98`

## RTK

Thank you to **Patrick Szymkowiak** and the RTK contributors. Ocean's standalone
command minimizer inherits pytest-summary state-machine concepts through the
pinned Oh My Pi donor path. RTK is Apache-2.0 licensed at the audited revision.

- RTK: <https://github.com/rtk-ai/rtk>
- Audited donor revision: `878af7de99e0ba71da2e8fd996f6b52a1836e06c`

## Foundations

Ocean also depends on a much larger open-source ecosystem. Particular thanks
go to the maintainers and contributors of **Rust**, **Tokio**, **Axum**,
**Ratatui**, **Crossterm**, **serde**, **reqwest**, **rusqlite/SQLite**,
**tree-sitter**, and the many other crates represented in `Cargo.lock`.
Sibling Ocean surfaces additionally rely on projects including **Leptos** and
**Tauri**. Release distributions should carry a generated dependency-license
inventory rather than treating this human acknowledgement as a substitute for
package-level notices.

## No endorsement implied

Acknowledgement describes provenance and gratitude. It does not imply that any
person or upstream project endorses Ocean OS or Rising Tides.
