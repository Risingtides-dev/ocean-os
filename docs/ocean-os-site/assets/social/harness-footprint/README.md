# Harness footprint social assets

Two 1600×900 X images preserve a dated local measurement of Ocean and other locally installed agent harnesses. This is a point-in-time artifact, not a current or universal benchmark. The comparison deliberately counts both Ocean runtime executables instead of presenting only the smaller client.

## Measurement

Measured on Apple Silicon on 2026-07-18 using `du -sk` against each active install artifact. Values on the graphics use MiB (`KiB / 1024`) rounded to one decimal place.

| Harness | Active artifact | Version | KiB | MiB |
| --- | --- | --- | ---: | ---: |
| Ocean TUI | `~/.local/bin/ocean` | local `main` build | 11,568 | 11.3 |
| Ocean daemon | `target/release/ocean-daemon` | local `main` build | 34,568 | 33.8 |
| **Ocean total** | both required runtime binaries | local `main` build | **46,136** | **45.1** |
| OpenCode | Homebrew Cellar `opencode/1.17.15` | 1.17.15 | 131,984 | 128.9 |
| Pi | global `@earendil-works/pi-coding-agent` package | 0.80.10 | 177,076 | 172.9 |
| Goose | `~/.local/bin/goose` | 1.36.0 | 236,976 | 231.4 |
| Claude Code | active version executable | 2.1.214 | 241,304 | 235.6 |
| Codex | global `@openai/codex` package | 0.144.5 | 304,416 | 297.3 |

The smallest comparison install in this 2026-07-18 sample is OpenCode 1.17.15 at 128.9 MiB. Ocean's 45.1 MiB total is 65.0% smaller by `(1 - 45.1 / 128.9) × 100`.

## Files

- `01-local-footprint.png` — the lead comparison image.
- `02-count-the-daemon.png` — the transparent accounting image.
- Matching `.svg` files are the editable typography and layout sources.
- `backplate-comparison.png` and `backplate-runtime.png` are AI-generated, text-free product backplates.

## Generation prompts

`backplate-comparison.png` used the built-in OpenAI image generator with an editorial industrial comparison scene: one compact graphite and ice-cyan runtime module against five oversized matte-charcoal harness housings in a dark navy studio. The prompt reserved upper-left space for typography and prohibited text, logos, purple, neon, sparkles, people, UI, and generic SaaS styling.

`backplate-runtime.png` used the built-in OpenAI image generator with an exploded two-module runtime: one compact client module above a roughly three-times-larger daemon module, both in graphite with restrained ice-cyan edges. The prompt reserved the left half for typography and applied the same text, branding, color, UI, and decoration exclusions.

Exact numbers, harness names, footnotes, and accessibility descriptions are rendered from the SVG sources rather than generated into the backplates.

## Evidence and reproduction

- Classification: dated point-in-time local measurement under [`../../../PUBLIC_EVIDENCE_POLICY.md`](../../../PUBLIC_EVIDENCE_POLICY.md); do not present these values as current or universal.
- The PNGs and SVGs contain no usernames, hostnames, local paths, session identifiers, private project names, or generated text/logos.
- Re-render at 1600×900 with Chrome's SVG engine: `"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless --hide-scrollbars --disable-gpu --force-device-scale-factor=1 --window-size=1600,900 --screenshot=<output.png> file://"$PWD/<source.svg>"`. Do not use ImageMagick's SVG renderer (it collapses the system-font layout), and do not use `sips` while the sources retain SVG filters that CoreSVG does not support.
- Validate dimensions with `sips -g pixelWidth -g pixelHeight *.png` and recompute the MiB/percentage arithmetic from the table above before publishing.
