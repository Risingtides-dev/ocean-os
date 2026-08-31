#!/usr/bin/env node
// Refreshes the checked-in copy of ocean-bedrock's published room-event set,
// which `crates/ocean-daemon/src/room_federation.rs` holds
// `BEDROCK_ROOM_WORKSPACE_EVENTS` equal to on every `cargo test`.
//
// WHY A COPY AND NOT A FETCH. ocean-bedrock is PRIVATE; ocean-os is PUBLIC. A
// workflow here could check out or fetch that sibling only on a cross-repo
// token held in this repo's secrets, which this project has not taken on for a
// staleness check — so the pin is not a CI step reaching across the two, and
// refreshing it stays this command. A committed artifact instead makes the
// assertion ALWAYS run: no sibling checkout, no network, no skip-when-absent
// branch that quietly stops asserting on the day it matters. What that buys is
// narrow and worth being exact about — the daemon's allowlist can no longer
// drift from the copy. The copy can still go stale against a newer Bedrock,
// and only running this command fixes that. That is the whole reason this file
// exists: before it, the pinned set was hand-typed under a comment naming a
// sha, with no written way to re-derive it, and a phantom action (`mkdir`)
// survived in that list for as long as the list existed.
//
// WHAT IT REPORTS. A moved action set is not a mechanical update. Anything
// ocean-bedrock adds arrives here UNRULED, and the daemon's default for an
// unknown action is to drop it silently — so this prints the delta and names
// the partitions a human has to rule on before the suite goes green again.
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { copyFile, readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

const FIXTURE_DIR = 'crates/ocean-daemon/tests/fixtures/bedrock-room-events';
const VENDORED = path.join(FIXTURE_DIR, 'room-event-actions.json');
const PROVENANCE = path.join(FIXTURE_DIR, 'vendored-from.json');
const UPSTREAM = 'docs/room-event-actions.json';

const HELP = `Usage:
  node scripts/vendor-bedrock-room-events.mjs [checkout]   refresh the copy
  node scripts/vendor-bedrock-room-events.mjs --check      compare, write nothing

Copies ${UPSTREAM} out of an ocean-bedrock checkout into
${VENDORED}, and stamps the commit it came
from into ${PROVENANCE}.

The checkout is the first argument, else $OCEAN_BEDROCK_DIR, else the sibling
../ocean-bedrock. Exit 0 when the copy is current, 1 when --check finds drift,
2 when the check could not run at all.`;

// The shape ocean-bedrock publishes. Validated rather than trusted, because a
// silently empty `actions` array would vendor cleanly and then assert nothing.
export function publishedActions(text, where) {
  let doc;
  try {
    doc = JSON.parse(text);
  } catch (err) {
    throw new Error(`${where} is not JSON: ${err.message}`);
  }
  if (doc?.produced_by !== 'ocean-bedrock') {
    throw new Error(`${where} is not ocean-bedrock's artifact (produced_by: ${doc?.produced_by})`);
  }
  const actions = doc.actions;
  const usable = Array.isArray(actions) && actions.length > 0 && actions.every((a) => typeof a === 'string');
  if (!usable) throw new Error(`${where} carries no usable \`actions\` array`);
  return actions;
}

export function drift(before, after) {
  const had = new Set(before);
  const has = new Set(after);
  return {
    added: after.filter((action) => !had.has(action)),
    removed: before.filter((action) => !has.has(action)),
  };
}

export function provenanceText({ sha, dirty, checkedOn }) {
  const stamp = {
    note: "Which ocean-bedrock commit the room-event-actions.json beside this file was copied from. Written by scripts/vendor-bedrock-room-events.mjs; re-run that rather than hand-editing. ocean-bedrock is private, so this sha is the whole of the provenance a reader of this public repo can be given.",
    repo: 'ocean-bedrock',
    path: UPSTREAM,
    sha,
    // A dirty checkout means the sha describes the commit and NOT the bytes
    // copied beside it — provenance that reads true and is not — so it is
    // recorded here rather than warned about once and forgotten.
    checkout_was_dirty: dirty,
    checked_on: checkedOn,
  };
  return `${JSON.stringify(stamp, null, 2)}\n`;
}

// `pipe` on stderr rather than the default inherit: the failure this reports
// is "no checkout there", and git's own `fatal: cannot change to ...` printed
// above that answer just buries it.
function git(dir, args) {
  return execFileSync('git', ['-C', dir, ...args], {
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
  }).trim();
}

// Exit-2 territory: every way the checkout can be unusable, each answered with
// where to get it. A reader of the public repo has no path to the private one,
// so "clone it" is not advice this can honestly give.
function resolveCheckout(dir) {
  let origin;
  try {
    origin = git(dir, ['remote', 'get-url', 'origin']);
  } catch {
    throw new Error(
      `No ocean-bedrock checkout at ${dir}.\n` +
        'Point OCEAN_BEDROCK_DIR at one, or pass it as the first argument.\n' +
        'ocean-bedrock is a PRIVATE repo: this needs a checkout you already\n' +
        'have, and there is no public URL to fall back to.',
    );
  }
  if (!origin.includes('ocean-bedrock')) {
    throw new Error(`${dir} is a git checkout of ${origin}, not ocean-bedrock`);
  }
  return {
    sha: git(dir, ['rev-parse', 'HEAD']),
    dirty: git(dir, ['status', '--porcelain', '--', UPSTREAM]) !== '',
  };
}

export async function main(argv = [], env = {}, log = console.log) {
  if (argv.includes('--help') || argv.includes('-h')) {
    log(HELP);
    return 0;
  }
  const check = argv.includes('--check');
  const named = argv.filter((arg) => !arg.startsWith('--'));

  const checkout = path.resolve(
    named[0] ?? env.OCEAN_BEDROCK_DIR ?? path.join(repoRoot, '..', 'ocean-bedrock'),
  );
  let head;
  let upstreamBytes;
  let vendoredBytes;
  let upstream;
  let vendored;
  try {
    head = resolveCheckout(checkout);
    const where = path.join(checkout, UPSTREAM);
    upstreamBytes = await readFile(where);
    upstream = publishedActions(upstreamBytes.toString('utf8'), where);
    vendoredBytes = await readFile(path.join(repoRoot, VENDORED));
    vendored = publishedActions(vendoredBytes.toString('utf8'), VENDORED);
  } catch (err) {
    log(err.message);
    return 2;
  }

  const { added, removed } = drift(vendored, upstream);
  const moved = added.length > 0 || removed.length > 0;
  const dirty = head.dirty ? ' (dirty)' : '';
  log(`ocean-bedrock ${head.sha.slice(0, 7)}${dirty} publishes ${upstream.length} actions`);
  for (const action of added) log(`  + ${action}  (new upstream — DROPPED SILENTLY here today)`);
  for (const action of removed) log(`  - ${action}  (no longer published)`);

  if (check) {
    // The action set agreeing is NOT the claim this file makes. `copyFile`
    // vendors the artifact verbatim, so the note, the source line and the
    // prefix are pinned too — and a hand-edit to any of them moves no action
    // and would otherwise be reported as current, which is a false green on
    // exactly the byte identity the fixture is evidence for.
    const same = upstreamBytes.equals(vendoredBytes);
    if (!moved && !same) log(`  the action sets agree, but the bytes differ from ${UPSTREAM}`);
    log(moved || !same ? `${VENDORED} is stale — re-run without --check.` : `${VENDORED} is current.`);
    return moved || !same ? 1 : 0;
  }

  await copyFile(path.join(checkout, UPSTREAM), path.join(repoRoot, VENDORED));
  const checkedOn = new Date().toISOString().slice(0, 10);
  await writeFile(path.join(repoRoot, PROVENANCE), provenanceText({ ...head, checkedOn }));
  log(`Wrote ${VENDORED} and ${PROVENANCE}.`);
  if (moved) {
    log('');
    log('The set MOVED, so `cargo test -p ocean-daemon` now fails on purpose.');
    log('Rule on each action above, in crates/ocean-daemon/src/room_federation.rs:');
    log('  BEDROCK_ROOM_WORKSPACE_EVENTS  the pinned set the fixture holds equal');
    log('  ADMITTED / DELIBERATE_NOISE    every action lands in exactly one');
    log('  workspace_action_is_marker     and ADMITTED must be what it matches');
  }
  return 0;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  process.exitCode = await main(process.argv.slice(2), process.env);
}
