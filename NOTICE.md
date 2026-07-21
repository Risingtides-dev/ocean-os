# Third-party notices

Ocean OS includes, ports, reimplements, or adapts material from the projects
listed below. We are grateful to their authors and contributors. Human-readable
acknowledgements are in [`CREDITS.md`](CREDITS.md).

## Pi agent/runtime baseline

`ocean-runtime` and `ocean-protocol` began from `pi-agent` and `pi-ai` version
1.0.0, published on crates.io as MIT-licensed Rust ports of the Pi agent core
and provider layer:

- <https://crates.io/crates/pi-agent/1.0.0>
- <https://crates.io/crates/pi-ai/1.0.0>
- <https://github.com/earendil-works/pi>

The crates were published by Naoki Takata. The linked Pi repository's MIT
license carries this notice:

    Copyright (c) 2025 Mario Zechner

Ocean adopted those sources into its own tree and has changed their
architecture substantially. The upstream MIT terms remain applicable to the
upstream-derived portions; Ocean's original changes are © 2026 Rising Tides
under the project license.

## Oh My Pi

Ocean contains ports, reimplementations, adapted tests, or source-informed
mechanisms from the MIT-licensed Oh My Pi project:

- Upstream: <https://github.com/can1357/oh-my-pi>
- Principal audited revision: `03c48d073bd4849726cc14750b5aecfa310bdf26`
- Copyright (c) 2025 Mario Zechner
- Copyright (c) 2025-2026 Can Bölük

Affected areas include hashline editing, LSP/code-intelligence behavior,
structural code summaries, OAuth and provider behavior, filesystem walking and
search, command-output minimization, runtime retry/tool scheduling, and
selected TUI history, diff, and Markdown interaction patterns. Narrow donor
paths and exclusions are recorded in source headers and crate-local notices
where available, especially under `ocean-minimizer`, `ocean-walker`, and
`ocean-search`.

## inertia-tui

The 3D camera and projection mathematics in
`crates/ocean-tui/src/shell/spatial.rs` are reimplemented and adapted from the
MIT OR Apache-2.0 licensed inertia-tui project. Ocean elects the MIT terms for
this donor material.

- Upstream: <https://github.com/aclfe/inertia>
- Audited donor revision: `99196825d5f62bd7524485c411f4b1e58d4f8a98`
- Copyright (c) 2026 Kairav Mittal

The adapted scope is the world-to-camera basis transform, perspective
projection, orbit/pan/zoom camera controls, and 2:1 terminal-cell aspect
correction. Ocean uses local graph-specific data structures and rendering.

## RTK

The standalone `ocean-minimizer` crate inherits pytest-summary state-machine
concepts through its pinned Oh My Pi donor from RTK:

- Upstream: <https://github.com/rtk-ai/rtk>
- Audited revision: `878af7de99e0ba71da2e8fd996f6b52a1836e06c`
- Upstream path: `src/cmds/python/pytest_cmd.rs`
- Copyright (c) 2024 Patrick Szymkowiak
- License: Apache License 2.0

Ocean's version is modified and reimplemented for its bounded,
already-tokenized minimizer. RTK had no `NOTICE` file at the audited revision.
The Apache License 2.0 text is distributed in [`LICENSE-APACHE`](LICENSE-APACHE).

## MIT license text for the MIT donor material above

MIT License

Copyright (c) 2025 Mario Zechner
Copyright (c) 2025-2026 Can Bölük
Copyright (c) 2026 Kairav Mittal

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## Dependency and asset notices

Rust dependencies remain under the licenses declared by their packages and
recorded in `Cargo.lock`; release artifacts should include a generated
package-level license inventory. Sibling Ocean repositories carry their own
notices for UI dependencies and bundled assets such as Lucide icons and
Poppins fonts.

Third-party names and marks belong to their respective owners. Inclusion here
does not imply endorsement of Ocean OS or Rising Tides.
