#!/usr/bin/env node
// Answers one question: does every events.md entry still close with its `___`
// rule? The union merge driver (.gitattributes) loses exactly one rule per
// extra parallel append — both sides end their entry with the SAME trailing
// rule the entry before them ends with, so xdiff anchors each insertion before
// that shared line and union emits both bodies with the one rule they share.
// The two entries then fuse: the second `time:` header lands directly under the
// first entry's prose. No conflict, no marker, nothing a merge check would see.
// So the loop runs this before and after every rebase and diffs the verdict.
//
// Ported from ocean-bedrock's scripts/check-ledger.mjs (its PR #62, commit
// 09bb1bb). Every executable line is byte-identical to that copy; only comments
// and the usage text differ, and the usage text only because it can name no npm
// script — this repo has no node manifest to hold one. Keeping the code itself
// identical is deliberate: a fix to either copy ports to the other as a patch.
//
// TWO THINGS THIS CHECK DELIBERATELY DOES NOT DO:
//   Compare totals. Rule lines against entry count is a different question and
//   it lies in both directions at once: this ledger held 606 rules against 511
//   entries when the check first ran here — 95 MORE rules than entries — and
//   58 of those entries were open anyway, because 144 other entries carry a
//   second rule inside their prose. Only "is THIS entry closed before the next
//   one starts" holds everywhere.
//   Match a fixed rule width. Every rule this ledger has written is 81
//   underscores and the sibling repos use widths of their own; all of them are
//   the same rule, so the shape is what matters and never the length.
//
// ONE FALSE POSITIVE, ACCEPTED RATHER THAN CODED AROUND: this repo's events.md
// opens with a documentation header whose fenced schema block carries a `time:`
// line, so the template parses as entry #1 and every count printed here is one
// high. Teaching the parser to skip fenced blocks would fork this copy from
// bedrock's on the day it landed, into a branch bedrock's own ledger — which
// starts straight at an entry — could never exercise. Closing the header with a
// rule of its own costs one line, reads as the documentation/log boundary it
// already is, and keeps the two files diffable.
import path from 'node:path';
import { readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

const HELP = `Usage:
  node scripts/check-ledger.mjs                check this repo's events.md
  node scripts/check-ledger.mjs <path>         check another ledger
  node scripts/check-ledger.mjs [path] --fix   close every open entry in place

Reports every entry not closed by a \`___\` rule before the next \`time:\`
header. Exit 0 when the ledger is clean, 1 when an entry is open, 2 when the
check could not run — an unreadable file, or one holding no entries at all.`;

const ENTRY_HEADER = /^time:/;
const SEPARATOR_RULE = /^_{5,}[ \t]*$/;

// Only the fallback for a ledger with no rule left to copy: the width a repair
// writes is the width that file already uses, so a repaired entry stays
// byte-identical in shape to the entries around it.
const DEFAULT_RULE_WIDTH = 81;

function ruleWidth(lines) {
  const seen = new Map();
  for (const line of lines) {
    if (!SEPARATOR_RULE.test(line)) continue;
    const width = line.trimEnd().length;
    seen.set(width, (seen.get(width) || 0) + 1);
  }
  let width = DEFAULT_RULE_WIDTH;
  let commonest = 0;
  for (const [candidate, count] of seen) {
    if (count > commonest) {
      width = candidate;
      commonest = count;
    }
  }
  return width;
}

// Pure over the ledger text so the verdict is testable without a filesystem.
// `start`/`end` are 0-based half-open line indices; `line` and `runsInto` are
// what a human reads off an editor gutter.
export function readEntries(text) {
  const lines = text.split('\n');
  const starts = [];
  lines.forEach((line, index) => {
    if (ENTRY_HEADER.test(line)) starts.push(index);
  });
  return starts.map((start, n) => {
    const next = starts[n + 1];
    const end = next === undefined ? lines.length : next;
    return {
      start,
      end,
      line: start + 1,
      runsInto: next === undefined ? null : next + 1,
      header: lines[start].trim(),
      closed: lines.slice(start, end).some((line) => SEPARATOR_RULE.test(line)),
    };
  });
}

export function openEntries(text) {
  return readEntries(text).filter((entry) => !entry.closed);
}

// Inserts each missing rule after the entry's last non-blank line, plus the
// blank line the format puts between a rule and the next header when the fold
// ate that too. Repairs run back to front so the earlier indices stay valid.
export function closeEntries(text) {
  const lines = text.split('\n');
  const rule = '_'.repeat(ruleWidth(lines));
  const open = openEntries(text);
  for (const entry of [...open].reverse()) {
    let last = entry.end - 1;
    while (last > entry.start && lines[last].trim() === '') last--;
    const atEof = lines[last + 1] === undefined;
    const spaced = atEof || lines[last + 1] === '';
    lines.splice(last + 1, 0, ...(spaced ? [rule] : [rule, '']));
  }
  return { text: lines.join('\n'), closed: open };
}

function report(entries) {
  return entries.map(
    (entry) =>
      `  line ${String(entry.line).padStart(5)}  ${entry.header}  ` +
      (entry.runsInto === null ? 'runs to the end of the file' : `runs into the entry at line ${entry.runsInto}`),
  );
}

export async function main(argv = process.argv.slice(2)) {
  const fix = argv.includes('--fix');
  const args = argv.filter((arg) => arg !== '--fix');
  if (args.some((arg) => arg === '-h' || arg === '--help') || args.length > 1) {
    console.log(HELP);
    return args.length > 1 ? 2 : 0;
  }
  const file = path.resolve(args[0] || path.join(repoRoot, 'events.md'));
  let text;
  try {
    text = await readFile(file, 'utf8');
  } catch (error) {
    console.error(`check-ledger: cannot read ${file}: ${error.message}`);
    return 2;
  }

  const entries = readEntries(text);
  // A ledger with nothing that parses as an entry is not a clean ledger. It is
  // the wrong path, or a rebase that emptied the real one — and the sibling
  // repo has already seen a checkout holding 37 entries against master's 72.
  // Both read as "0 open" and the loop diffs this check's verdict either side
  // of a rebase, so the one thing it must never do is answer that green.
  if (!entries.length) {
    console.error(`check-ledger: ${file} holds no \`time:\` entries — wrong path, or a ledger that lost its contents`);
    return 2;
  }

  const open = entries.filter((entry) => !entry.closed);
  if (!open.length) {
    console.log(`${file}: ${entries.length} entries, every one closed by its rule`);
    return 0;
  }

  if (fix) {
    await writeFile(file, closeEntries(text).text);
    console.log(`${file}: closed ${open.length} of ${entries.length} entries`);
    for (const line of report(open)) console.log(line);
    return 0;
  }
  console.log(
    `${file}: ${open.length} of ${entries.length} entries are not closed by a rule before the next entry begins`,
  );
  for (const line of report(open)) console.log(line);
  console.log('the union merge driver drops one rule per parallel append — rerun with --fix to close them');
  return 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  process.exitCode = await main();
}
