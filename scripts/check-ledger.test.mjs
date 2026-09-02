// Covers scripts/check-ledger.mjs. ocean-bedrock exercises the same logic from
// the other end — it merges two branches with the real union driver and asserts
// the checker sees the fold, and that the identity separator stops it — but that
// test needs a node manifest and a git harness this repo has none of. These are
// pure over the text instead, plus the exit contract, which is the part CI
// actually depends on.
//
// Run: node --test scripts/check-ledger.test.mjs
import assert from 'node:assert/strict';
import test from 'node:test';
import os from 'node:os';
import path from 'node:path';
import { mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

import {
  CODE_DIGEST,
  CODE_REVISION,
  closeEntries,
  codeDigest,
  entryIdentity,
  main,
  openEntries,
  readEntries,
} from './check-ledger.mjs';

const RULE = '_'.repeat(81);

// `worktree` is a parameter because the identity a repair writes is read off
// this line: an entry on a branch closes with its minute AND its branch, one
// written on the main checkout with the minute alone. Pass null for an entry
// carrying no `worktree:` field at all.
function entry(time, prose, worktree = 'main') {
  return [
    `time:      [${time}] [01-01-26]`,
    'agent:     [test]',
    ...(worktree === null ? [] : [`worktree:  ${worktree}`]),
    '',
    prose,
  ];
}

// A healthy ledger: every entry closed by its rule, one blank line between the
// rule and the next header.
const CLEAN = [...entry('10:00', 'First.'), RULE, '', ...entry('11:00', 'Second.'), RULE, ''].join('\n');

// What the union driver actually leaves behind: the rule the two entries shared
// is gone, so the second header sits directly under the first entry's prose.
const FOLDED = [...entry('10:00', 'First.'), ...entry('11:00', 'Second.'), RULE, ''].join('\n');

// Every line of `before` still present, in order, in `after`. The repair is a
// tail edit on an append-only file, so proving it deletes nothing matters more
// than proving what it inserted.
function isSubsequence(before, after) {
  let i = 0;
  for (const line of after) if (i < before.length && line === before[i]) i++;
  return i === before.length;
}

async function tempLedger(text) {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'check-ledger-'));
  const file = path.join(dir, 'events.md');
  await writeFile(file, text);
  return file;
}

test('readEntries splits on time: headers and reports 1-based gutter lines', () => {
  const entries = readEntries(CLEAN);
  assert.equal(entries.length, 2);
  assert.equal(entries[0].line, 1);
  assert.equal(entries[0].runsInto, 8);
  assert.equal(entries[1].runsInto, null, 'the last entry runs to the end of the file');
  assert.ok(entries.every((e) => e.closed));
});

test('a fold leaves the first entry open and names where the next one starts', () => {
  const open = openEntries(FOLDED);
  assert.equal(open.length, 1);
  assert.equal(open[0].line, 1);
  assert.equal(open[0].runsInto, 6, 'the second header lands directly under the first prose');
});

// All 697 rules this ledger carried before the convention are bare, and it is
// append-only, so the bare form has to keep closing an entry for as long as the
// file exists.
test('both exact rule forms close an entry', () => {
  const bare = [...entry('10:00', 'First.'), RULE, ''];
  const identity = [...entry('11:00', 'Second.'), `${RULE} 11:00 loop/slice-b`, ''];
  assert.equal(openEntries([...bare, ...identity].join('\n')).length, 0);
});

test('an embedded separator does not close an entry', () => {
  // A separator-shaped quotation in the prose must not conceal a later fold.
  // Only the final nonblank line before the next entry closes the entry.
  const quoted = [...entry('10:00', 'First.'), RULE, 'Quoting the rule above, not closing on it.', ''];
  const ledger = [...quoted, ...entry('11:00', 'Second.'), RULE].join('\n');
  const open = openEntries(ledger);
  assert.equal(open.length, 1);
  assert.equal(open[0].line, 1);
  assert.notEqual(quoted.filter((line) => line.trim()).pop(), RULE, 'the fixture must not end on a rule');
});

test('prose after the bar is not an identity separator', () => {
  const ledger = [...entry('10:00', 'First.'), `${RULE} arbitrary prose`, ''].join('\n');
  assert.equal(openEntries(ledger).length, 1);
});

test('entryIdentity reads the minute and the worktree off the entry itself', () => {
  assert.equal(entryIdentity(entry('09:04', 'On a branch.', 'loop/slice-a')), '09:04 loop/slice-a');
  assert.equal(entryIdentity(entry('9:04', 'A historical one-digit hour.', 'loop/slice-a')), '09:04 loop/slice-a');
  assert.equal(entryIdentity(entry('09:04', 'On the main checkout.', null)), '09:04', 'no branch to name');
  assert.equal(
    entryIdentity(['time:      no clock here', 'worktree:', '', 'Neither field carries a value.']),
    '',
    'an entry naming neither a time nor a branch has no identity to write',
  );
});

test('a repaired one-digit-hour entry is recognized as closed on the rerun', () => {
  const open = entry('9:04', 'This entry lost its separator.', 'loop/slice-a').join('\n');
  const repaired = closeEntries(open).text;
  assert.match(repaired, /^_{5,} 09:04 loop\/slice-a$/m);
  assert.equal(openEntries(repaired).length, 0);
});

test('an invalid header clock never becomes an invalid identity separator', () => {
  for (const invalid of ['24:00', '99:99', '09:60']) {
    const open = entry(invalid, 'This malformed entry lost its separator.', 'loop/slice-a').join('\n');
    assert.equal(entryIdentity(open.split('\n')), '', `${invalid} is not a 24-hour clock`);
    const repaired = closeEntries(open).text;
    assert.match(repaired, /^_{5,}$/m, 'the repair falls back to the valid bare form');
    assert.equal(openEntries(repaired).length, 0, 'the repaired entry is closed on the next run');
  }
});

test('closeEntries repairs the fold without deleting a line, and the rerun is clean', () => {
  const { text, closed } = closeEntries(FOLDED);
  assert.equal(closed.length, 1);
  assert.equal(openEntries(text).length, 0);

  const before = FOLDED.split('\n');
  const after = text.split('\n');
  assert.ok(after.length > before.length, 'the repair inserts');
  assert.ok(isSubsequence(before, after), 'the repair deletes nothing');
  // The identity form, not a bare rule: a repair that wrote the bare one would
  // close this entry and hand the next merge the same shared line to fold on.
  assert.equal(after[5], `${RULE} 10:00 main`, "the repaired rule carries the first entry's own identity");
  assert.deepEqual(
    after.slice(6, 8),
    ['', 'time:      [11:00] [01-01-26]'],
    'and the blank line the fold ate comes back with it, before the header it was fused into',
  );
});

// The property the whole port buys, at the width the loop actually runs: two
// slices appending in parallel must not end on the same line, or union emits
// that line once and fuses them again.
test('two entries repaired in the same minute close with rules that differ', () => {
  const folded = [
    ...entry('09:00', 'The entry both slices branched from.'),
    RULE,
    '',
    ...entry('12:30', 'Slice A.', 'loop/slice-a'),
    ...entry('12:30', 'Slice B.', 'loop/slice-b'),
  ].join('\n');
  const repaired = closeEntries(folded).text.split('\n');
  const rules = repaired.filter((line) => /^_{5,}/.test(line));
  assert.equal(rules.length, 3, "the base entry's bare rule plus one per repaired entry");
  assert.equal(new Set(rules).size, 3, 'the minute is shared, so the worktree is the whole of the identity here');
});

test('the repair copies the rule width the file already uses, measuring the bar and not the line', () => {
  const narrow = '_'.repeat(73);
  // The file's own rules already carry identity. Measuring the whole line
  // instead of the underscore run would read this as a 73 + suffix width and
  // widen every later repair away from the shape the file uses.
  const folded = [
    ...entry('10:00', 'First.'),
    ...entry('11:00', 'Second.'),
    `${narrow} 11:00 main`,
    '',
  ].join('\n');
  const repaired = closeEntries(folded).text.split('\n');
  assert.equal(repaired[5], `${narrow} 10:00 main`);
  assert.ok(!repaired.some((line) => line.startsWith(RULE)), 'never the default width when the file has one of its own');
});

test('a ledger whose every entry is open is fully repaired in one pass', () => {
  const open = [...entry('10:00', 'First.'), '', ...entry('11:00', 'Second.')].join('\n');
  assert.equal(openEntries(open).length, 2);
  assert.equal(openEntries(closeEntries(open).text).length, 0);
});

// This repo's events.md opens with a documentation header whose fenced schema
// block carries a `time:` line. The parser has no idea it is inside a fence, so
// the template reads as entry #1. That is accepted rather than coded around —
// see the script header — and the accepted behaviour is pinned here so nobody
// later reads it as a bug and forks this copy from bedrock's to "fix" it.
test('the fenced schema template parses as an entry, and a rule closes it', () => {
  // The real header's shape, placeholders included: its clock is a literal
  // `[HH:MM]`, which is not a time and never matches, and its `worktree:` line
  // holds a placeholder rather than a branch.
  const header = [
    '# ocean-os — canonical repo ledger',
    '',
    '**Schema — required fields:**',
    '',
    '```',
    'time:      [HH:MM] [dd-mm-yy]  (24-hour, EST UTC-4 — never am/pm)',
    'agent:     [harness], [model-id], [persona]*  (* if known)',
    'worktree:  [branch/ref] or [main]   (required on every entry)',
    '```',
    '',
    '---',
    '',
  ];
  const withHeader = [...header, ...entry('10:00', 'First.'), RULE, ''].join('\n');

  const open = openEntries(withHeader);
  assert.equal(open.length, 1, 'the template, not the real entry');
  assert.equal(open[0].line, 6);

  const repaired = closeEntries(withHeader).text.split('\n');
  assert.equal(
    repaired[11],
    RULE,
    'the rule lands after the --- that already divides header from log, and stays bare because `[HH:MM]` is not a clock',
  );
  assert.equal(openEntries(repaired.join('\n')).length, 0);
});

test('main exits 0 on a clean ledger and 1 on an open one', async () => {
  assert.equal(await main([await tempLedger(CLEAN)]), 0);
  assert.equal(await main([await tempLedger(FOLDED)]), 1);
});

test('main --fix closes the ledger in place with the identity form, and exits 0', async () => {
  const file = await tempLedger(FOLDED);
  assert.equal(await main([file, '--fix']), 0);
  const repaired = await readFile(file, 'utf8');
  assert.equal(openEntries(repaired).length, 0);
  assert.match(repaired, /^_{5,} 10:00 main$/m, 'the rule --fix wrote names the entry it closes');
  assert.equal(await main([file]), 0, 'and the plain rerun now agrees');
});

test('main exits 2 when the check could not run at all', async () => {
  assert.equal(await main([await tempLedger('no entries here, just prose\n')]), 2, 'a ledger that lost its contents');
  assert.equal(await main([path.join(os.tmpdir(), 'check-ledger-does-not-exist', 'events.md')]), 2, 'an unreadable path');
  assert.equal(await main(['one.md', 'two.md']), 2, 'more paths than it can check');
});

test('--help exits 0 and names the exit contract', async () => {
  assert.equal(await main(['--help']), 0);
});

const KNOWN_STAMPS = {
  r1: 'de98a632f0df',
  r2: '56adab136337',
  r3: 'c15369d1f68c',
  r4: '4762696f29d4',
};

test('check-ledger.mjs carries the current shared code stamp', async () => {
  const source = await readFile(fileURLToPath(new URL('check-ledger.mjs', import.meta.url)), 'utf8');
  assert.equal(codeDigest(source), CODE_DIGEST);
  assert.equal(KNOWN_STAMPS[CODE_REVISION], CODE_DIGEST);
  assert.match(CODE_REVISION, /^r\d+$/);
  assert.ok(source.includes(`'${CODE_DIGEST}'`), 'the digest remains a grep-readable literal');
});
