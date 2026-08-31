// Covers scripts/vendor-bedrock-room-events.mjs, and — the part that matters
// on a day nobody is running this — the vendored fixture it maintains.
//
// Pure over the exported functions plus the exit contract, the same shape
// check-ledger.test.mjs takes and for the same reason: this repo has no node
// manifest, and a git harness for a PRIVATE sibling is not something a public
// repo's test can conjure. The one cross-repo assertion — that the pinned Rust
// set equals the fixture — belongs to `cargo test -p ocean-daemon`, which runs
// on every build; this file only proves the refresh path around it.
//
// Run: node --test scripts/vendor-bedrock-room-events.test.mjs
// (.github/workflows/ci.yml names its node test files one by one and adding
// this one there is a different slice's file to touch.)
import assert from 'node:assert/strict';
import test from 'node:test';
import path from 'node:path';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

import { drift, main, provenanceText, publishedActions } from './vendor-bedrock-room-events.mjs';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const FIXTURE_DIR = path.join(repoRoot, 'crates/ocean-daemon/tests/fixtures/bedrock-room-events');

function artifact(overrides = {}) {
  return JSON.stringify({
    produced_by: 'ocean-bedrock',
    prefix: 'room.workspace.',
    actions: ['room.workspace.flushed', 'room.workspace.hydrated'],
    ...overrides,
  });
}

test('an artifact from anywhere but ocean-bedrock is refused', () => {
  assert.throws(() => publishedActions(artifact({ produced_by: 'ocean-surface' }), 'x'), /not ocean-bedrock/);
  assert.throws(() => publishedActions('{', 'x'), /not JSON/);
});

// An empty or non-string `actions` array would vendor without complaint and
// then hold the Rust pin equal to nothing, which is the one failure this whole
// mechanism exists to prevent.
test('an actions array that would assert nothing is refused', () => {
  assert.throws(() => publishedActions(artifact({ actions: [] }), 'x'), /no usable/);
  assert.throws(() => publishedActions(artifact({ actions: {} }), 'x'), /no usable/);
  assert.throws(() => publishedActions(artifact({ actions: ['ok', 7] }), 'x'), /no usable/);
});

test('drift names both directions', () => {
  const { added, removed } = drift(['a', 'b'], ['b', 'c']);
  assert.deepEqual(added, ['c']);
  assert.deepEqual(removed, ['a']);
  assert.deepEqual(drift(['a'], ['a']), { added: [], removed: [] });
});

test('provenance records the sha, the dirty flag and the date', () => {
  const stamp = JSON.parse(provenanceText({ sha: 'abc123', dirty: true, checkedOn: '2026-08-31' }));
  assert.equal(stamp.repo, 'ocean-bedrock');
  assert.equal(stamp.sha, 'abc123');
  assert.equal(stamp.checkout_was_dirty, true);
  assert.equal(stamp.checked_on, '2026-08-31');
});

// Exit 2 is "could not run", and it must never be confused with exit 0. A
// missing private sibling is the ordinary case for anyone who is not on the
// team, so it has to say where the checkout comes from rather than half-work.
test('a missing checkout fails loudly and cannot be mistaken for success', async () => {
  const said = [];
  const code = await main(['--check'], { OCEAN_BEDROCK_DIR: '/nonexistent/ocean-bedrock' }, (l) => said.push(l));
  assert.equal(code, 2);
  assert.match(said.join('\n'), /No ocean-bedrock checkout at \/nonexistent\/ocean-bedrock/);
  assert.match(said.join('\n'), /PRIVATE/);
});

test('a checkout of the wrong repo is refused', async () => {
  const said = [];
  const code = await main(['--check', repoRoot], {}, (l) => said.push(l));
  assert.equal(code, 2);
  assert.match(said.join('\n'), /not ocean-bedrock/);
});

// The fixture is the thing every `cargo test` reads, so it is worth asserting
// on its own terms and not only through the refresh path.
test('the checked-in fixture is a usable ocean-bedrock artifact', async () => {
  const text = await readFile(path.join(FIXTURE_DIR, 'room-event-actions.json'), 'utf8');
  const actions = publishedActions(text, 'fixture');
  // Spelled out rather than read back out of the artifact under test: the
  // namespace is bedrock's own hardcoded ROOM_EVENT_PREFIX, and a fixture that
  // widened this key would otherwise satisfy the loop below trivially.
  const ROOM_EVENT_PREFIX = 'room.workspace.';
  assert.equal(JSON.parse(text).prefix, ROOM_EVENT_PREFIX, 'the artifact publishes under the room-stream namespace');
  assert.ok(actions.every((action) => action.startsWith(ROOM_EVENT_PREFIX)), 'every action wears that namespace');
  assert.equal(new Set(actions).size, actions.length, 'no action is published twice');
});

test('the fixture is stamped with a full ocean-bedrock sha', async () => {
  const stamp = JSON.parse(await readFile(path.join(FIXTURE_DIR, 'vendored-from.json'), 'utf8'));
  assert.equal(stamp.repo, 'ocean-bedrock');
  assert.match(stamp.sha, /^[0-9a-f]{40}$/);
  assert.equal(stamp.checkout_was_dirty, false, 'a dirty checkout does not describe the bytes it shipped');
});
