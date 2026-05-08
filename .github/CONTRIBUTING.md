# Contributing to Ocean-OS

This repo is built collaboratively. Every Slack-bridge operator (and their Claude) is welcome to claim a piece and ship it.

## Claim an ingestion worker

The README has a table of sources. Each is independent — they don't share code, they don't share schemas (beyond the shape of the event log), they don't share deploys. Pick one:

1. Open an issue in this repo titled `claim: ingestion <source>` with a one-paragraph plan — what events you'll capture, how you'll get them (webhook vs polling), what the schema additions are.
2. Get a thumbs-up from at least one other operator in the issue.
3. Branch, build it under `ingestion/<source>/`, follow the GitHub worker as the reference implementation.
4. Open a PR. CI will lint and typecheck (when CI exists).
5. Once merged, you own deploys for that worker. The Ocean MCP exposes data through tools — your worker just has to land events in `<source>.events`.

## Add a tool to the MCP

If you want a new tool exposed to the bots:

1. Open an issue describing the tool — name, inputs, outputs, what it queries.
2. Add it under `mcp/src/tools/<tool_name>.ts` following the existing pattern.
3. Register it in `mcp/src/tools/index.ts`.
4. Update the README tool catalog.

## Schema changes

Schema lives in `schema/`. New schemas, new tables, new indexes — add a new SQL file with a sequence-prefixed name (`010_*`, `020_*`, etc.). Migrations run in filename order.

Don't touch existing files unless you're rebuilding a materialized view (which is a non-destructive operation by design — drop and recreate).

## Discussion

`#claude-ops` in the Rising Tides Slack. Anything architectural goes there before PR.
