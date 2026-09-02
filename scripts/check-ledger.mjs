#!/usr/bin/env node
// Answers one question: does every events.md entry still close with its `___`
// rule? Two entries that end with the SAME line are what the union merge driver
// (.gitattributes) cannot keep apart — it emits a line both sides added only
// once, so xdiff anchors each parallel append before the rule they share and one
// rule comes out for two entries. The entries then FUSE: the second `time:`
// header lands directly under the first entry's prose. No conflict, no marker,
// nothing a merge check would see. So a rule now carries its entry's own
// identity — `___ HH:MM <worktree>` — and two appends no longer end on the
// same line. The loop still runs this before and after every rebase and diffs
// the verdict, because the 697 rules written here before this convention are
// bare and identical to one another, and events.md is append-only: history
// keeps folding for as long as it is history. This is the largest of the three ledgers, so it pays that tax
// most often.
//
// Ported forward from ocean-bedrock's canonical checker through r4. A
// mechanically verified code stamp replaces the old byte-identity claim:
// comments and usage text remain repo-specific, while every behavior change
// must bump CODE_REVISION, CODE_DIGEST, and its regression-table row together.
//
// WHAT THIS CHECK OWNS, AND THE THREE NEIGHBOURING THINGS IT DOES NOT:
//   It owns FUSION — an entry whose rule is gone and whose prose the next
//   header runs into. That is the damage; everything below is cosmetic or out
//   of reach.
//   It does NOT own separator uniqueness. Requiring the identity form would red
//   every one of the 697 bare rules below it, and every entry a slice in
//   flight is writing right now. The bare form stays valid forever; `--fix` is
//   what writes the new one.
//   It does NOT own an entry's HEAD. Identity saves the tail: two entries
//   written in the same minute still open with the same blank line and the
//   same `time:` line, union folds that pair, and the second entry arrives
//   decapitated while this check — seeing one header closed by one rule —
//   exits 0. Being on two branches does not save it, and neither does a
//   differing `agent:`: both sit BELOW the two lines that collapse, and
//   same-minute appends on separate branches are this loop's normal mode. The
//   rule is the boundary this check can defend, not the whole entry.
//   It does NOT own the blank line between a rule and the next header, and the
//   ruling is that an entry owns its rule and not the blank after it. Measured
//   at one wave's land: union ate that blank on one repo's rebase and kept it
//   on another's within the hour, purely on where xdiff anchored. Losing it
//   costs nothing a reader or a parser needs — the rule already divides the
//   entries — so it is left unasserted rather than red on a merge that took
//   nothing. `--fix` still writes one back when it repairs.
//
// TWO THINGS THIS CHECK DELIBERATELY DOES NOT DO:
//   Compare totals. Rule lines against entry count is a different question and
//   it lies in both directions at once: this ledger held 606 rules against 511
//   entries WHEN THE CHECK FIRST RAN HERE — 95 MORE rules than entries — and
//   58 of those entries were open anyway, because other entries quote a rule
//   inside their own prose and so carry a second one. The surplus only grows
//   with the file; at this commit it is 699 rules against 545 entries, 145 of
//   which hold more than one. Only "is THIS entry closed before the next one
//   starts" holds everywhere.
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
// already is, and keeps the two files diffable. Note what identity makes of the
// template if it ever reopens: its clock is a literal `[HH:MM]` and never
// matches, but its `worktree:` line does carry a token, so a repair closes it
// with the PLACEHOLDER — `___ [branch/ref]` — rather than the bare rule the
// shape suggests. Harmless, because the template is closed and an append-only
// file does not reopen it, and pinned in the test rather than left to be
// discovered mid-rebase.
import path from 'node:path';
import { createHash } from 'node:crypto';
import { readFile, realpath, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

const scriptPath = fileURLToPath(import.meta.url);
const repoRoot = path.resolve(path.dirname(scriptPath), '..');

// The Bedrock checker is canonical. This stamp names the shared code shape,
// while comments and usage text remain repository-specific and are excluded.
export const CODE_REVISION = 'r4';
export const CODE_DIGEST = '4762696f29d4';

const STAMP_LINE = /^export const CODE_(?:REVISION|DIGEST) = /;
const USAGE_OPEN = /^const HELP = `/;
const USAGE_CLOSE = /`;$/;

function leadingLineComments(source) {
  const comments = new Set();
  const templateExpressions = [];
  let mode = 'code';
  let escaped = false;
  let line = 0;
  let lineHasCode = false;

  for (let index = 0; index < source.length; index += 1) {
    const char = source[index];
    const next = source[index + 1];
    if (char === '\n') {
      if (mode === 'line-comment') mode = 'code';
      line += 1;
      lineHasCode = false;
      escaped = false;
      continue;
    }
    if (mode === 'line-comment') continue;
    if (mode === 'block-comment') {
      if (!/\s/.test(char)) lineHasCode = true;
      if (char === '*' && next === '/') {
        mode = 'code';
        index += 1;
      }
      continue;
    }
    if (mode === 'single' || mode === 'double') {
      if (!/\s/.test(char)) lineHasCode = true;
      if (escaped) {
        escaped = false;
      } else if (char === '\\') {
        escaped = true;
      } else if ((mode === 'single' && char === "'") || (mode === 'double' && char === '"')) {
        mode = 'code';
      }
      continue;
    }
    if (mode === 'template') {
      if (!/\s/.test(char)) lineHasCode = true;
      if (escaped) {
        escaped = false;
      } else if (char === '\\') {
        escaped = true;
      } else if (char === '`') {
        mode = 'code';
      } else if (char === '$' && next === '{') {
        templateExpressions.push(1);
        mode = 'code';
        index += 1;
      }
      continue;
    }

    if (!lineHasCode && /[ \t\r]/.test(char)) continue;
    if (!lineHasCode && char === '/' && next === '/') {
      comments.add(line);
      mode = 'line-comment';
      index += 1;
      continue;
    }
    lineHasCode = true;
    if (char === '/' && next === '/') {
      mode = 'line-comment';
      index += 1;
    } else if (char === '/' && next === '*') {
      mode = 'block-comment';
      index += 1;
    } else if (char === "'") {
      mode = 'single';
    } else if (char === '"') {
      mode = 'double';
    } else if (char === '`') {
      mode = 'template';
    } else if (templateExpressions.length && char === '{') {
      templateExpressions[templateExpressions.length - 1] += 1;
    } else if (templateExpressions.length && char === '}') {
      const top = templateExpressions.length - 1;
      templateExpressions[top] -= 1;
      if (templateExpressions[top] === 0) {
        templateExpressions.pop();
        mode = 'template';
      }
    }
  }
  return comments;
}

export function codeDigest(source) {
  const body = [];
  const commentLines = leadingLineComments(source);
  let inUsage = false;
  for (const [index, raw] of source.split('\n').entries()) {
    const line = raw.replace(/\s+$/, '');
    if (inUsage || USAGE_OPEN.test(line)) {
      inUsage = !USAGE_CLOSE.test(line);
      continue;
    }
    if (line === '' || commentLines.has(index) || STAMP_LINE.test(line)) continue;
    body.push(line);
  }
  return createHash('sha256').update(body.join('\n')).digest('hex').slice(0, 12);
}

const HELP = `Usage:
  node scripts/check-ledger.mjs                check this repo's events.md
  node scripts/check-ledger.mjs <path>         check another ledger
  node scripts/check-ledger.mjs [path] --fix   close every open entry in place
  node scripts/check-ledger.mjs [copy] --digest print a checker copy's code digest

Reports every entry not closed by a \`___\` rule before the next \`time:\`
header. A rule may be bare or carry the entry's identity (\`___ HH:MM
<worktree>\`); both close an entry, and \`--fix\` writes the identity form.
Exit 0 when the ledger is clean, 1 when an entry is open, 2 when the
check could not run — an unreadable file, or one holding no entries at all.`;

const ENTRY_HEADER = /^time:/;
// Both forms, and the bare one forever: every rule written before the identity
// convention is bare, and an append-only ledger never stops carrying them.
const SEPARATOR_RULE = /^_{5,}(?:[ \t]+(?:[01]?\d|2[0-3]):[0-5]\d(?:[ \t]+\S+)?)?$/;
const RULE_BAR = /^_+/;
// What makes one entry's rule unlike its neighbours'. `HH:MM` alone is minute
// resolution and two slices in one wave land in the same minute often enough to
// have done it; the worktree is what the clock cannot give, and two parallel
// appends are by definition on two different branches. An entry with no
// worktree was written on the main checkout, where there is one writer and
// nothing to race, so its time alone is enough.
const HEADER_TIME = /\[((?:[01]?\d|2[0-3]):[0-5]\d)\]/;
const WORKTREE_FIELD = /^worktree:[ \t]*(\S+)/;

// Only the fallback for a ledger with no rule left to copy: the width a repair
// writes is the width that file already uses, so a repaired entry stays
// byte-identical in shape to the entries around it.
const DEFAULT_RULE_WIDTH = 81;

function ruleWidth(lines) {
  const seen = new Map();
  for (const line of lines) {
    if (!SEPARATOR_RULE.test(line)) continue;
    // The underscore RUN, not the line: an identity-bearing rule carries a
    // suffix, and measuring the whole line would widen every repair after it.
    const width = line.match(RULE_BAR)[0].length;
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
      closed: (() => {
        let tail = end - 1;
        while (tail > start && lines[tail].trim() === '') tail--;
        return SEPARATOR_RULE.test(lines[tail]);
      })(),
    };
  });
}

export function openEntries(text) {
  return readEntries(text).filter((entry) => !entry.closed);
}

// Read off the entry's own lines, so a repair is reproducible from the ledger
// and needs nothing about the checkout it runs in. An entry missing both fields
// gets the bare rule: the identity is a property of the entry, and one that
// names neither a time nor a branch has none to write.
export function entryIdentity(lines) {
  const parts = [];
  const time = lines[0]?.match(HEADER_TIME);
  if (!time) return '';
  // Historical ledger entries admit a one-digit hour, but new identity rules
  // use the repository's required HH:MM spelling so repairs do not perpetuate
  // the old format.
  const [hour, minute] = time[1].split(':');
  parts.push(`${hour.padStart(2, '0')}:${minute}`);
  const worktree = lines.map((line) => line.match(WORKTREE_FIELD)).find(Boolean);
  if (worktree) parts.push(worktree[1]);
  return parts.join(' ');
}

// Inserts each missing rule after the entry's last non-blank line, plus the
// blank line the format puts between a rule and the next header when the fold
// ate that too. The rule written is the IDENTITY form — a repair that emitted a
// bare one would close this entry and hand the next merge the same shared line
// to fold on. Repairs run back to front so the earlier indices stay valid.
export function closeEntries(text) {
  const lines = text.split('\n');
  const bar = '_'.repeat(ruleWidth(lines));
  const open = openEntries(text);
  for (const entry of [...open].reverse()) {
    let last = entry.end - 1;
    while (last > entry.start && lines[last].trim() === '') last--;
    const identity = entryIdentity(lines.slice(entry.start, entry.end));
    const rule = identity ? `${bar} ${identity}` : bar;
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
  const digest = argv.includes('--digest');
  const args = argv.filter((arg) => arg !== '--fix' && arg !== '--digest');
  if (args.some((arg) => arg === '-h' || arg === '--help') || args.length > 1) {
    console.log(HELP);
    return args.length > 1 ? 2 : 0;
  }
  const file = path.resolve(args[0] || (digest ? scriptPath : path.join(repoRoot, 'events.md')));
  let text;
  try {
    text = await readFile(file, 'utf8');
  } catch (error) {
    console.error(`check-ledger: cannot read ${file}: ${error.message}`);
    return 2;
  }

  if (digest) {
    console.log(`${codeDigest(text)}  ${file}`);
    return 0;
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
  console.log('union folds two entries that end with the SAME rule — rerun with --fix to close them by identity');
  return 1;
}

async function invokedAsScript() {
  if (!process.argv[1]) return false;
  try {
    return (await realpath(scriptPath)) === (await realpath(process.argv[1]));
  } catch {
    return false;
  }
}

if (await invokedAsScript()) {
  process.exitCode = await main();
}
