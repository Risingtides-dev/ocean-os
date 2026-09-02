// Covers the daemon-specific federation secret loader in deploy/ocean-daemon.sh
// and the file-writing half of ops/set-ocean-federation.sh, against a fake
// daemon binary that records which federation variables reached its process
// and a digest of the bearer, never the bearer itself. The launchd half of the
// activation script is not exercised here: it needs a real GUI domain, and the
// runbook's verification step covers it on the operated machine.
import assert from 'node:assert/strict';
import test from 'node:test';
import os from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { execFile } from 'node:child_process';
import { chmod, mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

const run = promisify(execFile);
const REPO = fileURLToPath(new URL('..', import.meta.url));
const WRAPPER = path.join(REPO, 'deploy', 'ocean-daemon.sh');
const SETTER = path.join(REPO, 'ops', 'set-ocean-federation.sh');
const TOKEN = 'obt_4f9c1d2e7a6b3c8d9e0f1a2b3c4d5e6f';
const sha16 = (s) => createHash('sha256').update(s).digest('hex').slice(0, 16);

async function harness() {
  const home = await mkdtemp(path.join(os.tmpdir(), 'ocean-fed-'));
  const bin = path.join(home, 'fake-daemon');
  const out = path.join(home, 'seen.txt');
  await writeFile(bin, `#!/bin/bash
{
  echo "URL_SET=\${OCEAN_FEDERATION_URL:+1}"
  echo "TOKEN_SET=\${OCEAN_FEDERATION_OWNER_TOKEN:+1}"
  echo "URL=\${OCEAN_FEDERATION_URL:-}"
  printf 'TOKEN_SHA=%s\\n' "$(printf %s "\${OCEAN_FEDERATION_OWNER_TOKEN:-}" | shasum -a 256 | cut -c1-16)"
} > "$OCEAN_TEST_OUT"
`);
  await chmod(bin, 0o755);
  const envFile = path.join(home, 'federation.env');
  return { home, bin, out, envFile };
}

async function launch(h, extraEnv = {}) {
  const env = { ...process.env, HOME: h.home, OCEAN_DAEMON_BIN: h.bin, OCEAN_TEST_OUT: h.out, OCEAN_FEDERATION_ENV_FILE: h.envFile, ...extraEnv };
  delete env.OCEAN_FEDERATION_URL; delete env.OCEAN_FEDERATION_OWNER_TOKEN;
  Object.assign(env, extraEnv);
  const { stdout, stderr } = await run('bash', [WRAPPER], { env });
  const seen = Object.fromEntries((await readFile(h.out, 'utf8')).trim().split('\n').map((l) => l.split(/=(.*)/s).slice(0, 2)));
  return { seen, log: stdout + stderr };
}

test('with no file, the daemon starts with federation off and no federation variable in its process', async () => {
  const h = await harness();
  const { seen, log } = await launch(h);
  assert.equal(seen.URL_SET, ''); assert.equal(seen.TOKEN_SET, '');
  assert.match(log, /federation=off/);
});

test('a 0600 file with the two keys reaches the daemon process, and the bearer never reaches the log', async () => {
  const h = await harness();
  await writeFile(h.envFile, `# comment\nOCEAN_FEDERATION_URL=https://bedrock.example\nOCEAN_FEDERATION_OWNER_TOKEN=${TOKEN}\n`, { mode: 0o600 });
  const { seen, log } = await launch(h);
  assert.equal(seen.URL, 'https://bedrock.example');
  assert.equal(seen.TOKEN_SHA, sha16(TOKEN));
  assert.match(log, /federation=on \(file\)/);
  assert.ok(!log.includes(TOKEN), 'the bearer must not appear in the launcher log');
});

test('a file that is not 0600 is refused whole, without printing key names or contents', async () => {
  const h = await harness();
  await writeFile(h.envFile, `OCEAN_FEDERATION_URL=https://bedrock.example\nOCEAN_FEDERATION_OWNER_TOKEN=${TOKEN}\n`, { mode: 0o644 });
  const { seen, log } = await launch(h);
  assert.equal(seen.URL_SET, ''); assert.equal(seen.TOKEN_SET, '');
  assert.match(log, /private configuration refused: unsafe_mode/);
  assert.ok(!log.includes(TOKEN));
  assert.ok(!log.includes('OCEAN_FEDERATION_'));
});

test('a line that is not one of the federation keys refuses the file without naming it', async () => {
  const h = await harness();
  await writeFile(h.envFile, `OCEAN_FEDERATION_URL=https://bedrock.example\nOCEAN_FEDERATION_OWNER_TOKEN=${TOKEN}\nOCEAN_YOLO=0\n`, { mode: 0o600 });
  const { seen, log } = await launch(h);
  assert.equal(seen.TOKEN_SET, '');
  assert.match(log, /private configuration refused: unsupported_entry/);
  assert.ok(!log.includes('OCEAN_YOLO'));
});

test('a URL with anything after the host, or a plain-http remote, is refused', async () => {
  for (const url of ['https://bedrock.example/api', 'http://bedrock.example', 'bedrock.example']) {
    const h = await harness();
    await writeFile(h.envFile, `OCEAN_FEDERATION_URL=${url}\nOCEAN_FEDERATION_OWNER_TOKEN=${TOKEN}\n`, { mode: 0o600 });
    const { seen, log } = await launch(h);
    assert.equal(seen.TOKEN_SET, '', url);
    assert.match(log, /private configuration refused: origin is invalid/);
  }
  const h = await harness();
  await writeFile(h.envFile, `OCEAN_FEDERATION_URL=http://127.0.0.1:4790\nOCEAN_FEDERATION_OWNER_TOKEN=${TOKEN}\n`, { mode: 0o600 });
  assert.equal((await launch(h)).seen.URL, 'http://127.0.0.1:4790', 'loopback http is allowed for local Bedrock');
});

test('an explicit process pair wins wholesale over the file fallback', async () => {
  const h = await harness();
  await writeFile(h.envFile, `OCEAN_FEDERATION_URL=https://disk.example\nOCEAN_FEDERATION_OWNER_TOKEN=${TOKEN}\n`, { mode: 0o600 });
  const { seen, log } = await launch(h, { OCEAN_FEDERATION_URL: 'https://process.example', OCEAN_FEDERATION_OWNER_TOKEN: 'process-secret' });
  assert.equal(seen.URL, 'https://process.example');
  assert.equal(seen.TOKEN_SHA, sha16('process-secret'));
  assert.match(log, /federation=on \(process\)/);
  assert.ok(!log.includes('process-secret'));
});

test('empty but present process variables suppress the disk fallback', async () => {
  const h = await harness();
  await writeFile(h.envFile, `OCEAN_FEDERATION_URL=https://disk.example\nOCEAN_FEDERATION_OWNER_TOKEN=${TOKEN}\n`, { mode: 0o600 });
  const { seen, log } = await launch(h, { OCEAN_FEDERATION_URL: '', OCEAN_FEDERATION_OWNER_TOKEN: '' });
  assert.equal(seen.URL, '', 'the disk URL must not replace an explicit empty process pair');
  assert.equal(seen.TOKEN_SET, '', 'the disk bearer must not replace an explicit empty process pair');
  assert.match(log, /federation=on \(process\)/);
  assert.ok(!log.includes(TOKEN));
});

test('a token and a keychain reference together are refused; a keychain reference alone defers to the Keychain', async () => {
  const h = await harness();
  await writeFile(h.envFile, `OCEAN_FEDERATION_URL=https://bedrock.example\nOCEAN_FEDERATION_OWNER_TOKEN=${TOKEN}\nOCEAN_FEDERATION_OWNER_TOKEN_KEYCHAIN=ocean-federation\n`, { mode: 0o600 });
  assert.match((await launch(h)).log, /private configuration refused: ambiguous_credential/);
  const h2 = await harness();
  await writeFile(h2.envFile, `OCEAN_FEDERATION_URL=https://bedrock.example\nOCEAN_FEDERATION_OWNER_TOKEN_KEYCHAIN=ocean-federation-test-missing\n`, { mode: 0o600 });
  const { seen, log } = await launch(h2);
  assert.equal(seen.TOKEN_SET, '');
  assert.match(log, /private configuration refused: credential_unavailable/);
  assert.ok(!log.includes('ocean-federation-test-missing'));
});

test('neither the tracked plist nor a rendered copy carries a federation key', async () => {
  const src = await readFile(path.join(REPO, 'deploy', 'dev.risingtides.ocean-daemon.plist'), 'utf8');
  assert.ok(!src.includes('OCEAN_FEDERATION'), 'tracked template');
  const rendered = src.replaceAll('__OCEAN_HOME__', '/Users/someone');
  assert.ok(!rendered.includes('OCEAN_FEDERATION'), 'rendered copy');
  assert.ok(!src.includes('launchctl setenv'), 'no domain-wide setenv anywhere near the job');
});

test('set-ocean-federation.sh --no-restart writes a 0600 file from --token-file, never taking the bearer on argv', async () => {
  const h = await harness();
  const tokenFile = path.join(h.home, 'bearer.txt');
  await writeFile(tokenFile, `${TOKEN}\n`, { mode: 0o600 });
  const env = { ...process.env, HOME: h.home, OCEAN_FEDERATION_ENV_FILE: h.envFile };
  const { stdout } = await run('bash', [SETTER, '--url', 'https://bedrock.example', '--token-file', tokenFile, '--no-restart'], { env });
  assert.match(stdout, /wrote .*federation\.env \(0600, bearer in the file\)/);
  assert.match(stdout, /launchd untouched/);
  const { mode } = await import('node:fs').then((fs) => fs.promises.stat(h.envFile));
  assert.equal(mode & 0o777, 0o600);
  const body = await readFile(h.envFile, 'utf8');
  assert.match(body, /^OCEAN_FEDERATION_URL=https:\/\/bedrock\.example$/m);
  assert.match(body, new RegExp(`^OCEAN_FEDERATION_OWNER_TOKEN=${TOKEN}$`, 'm'));
  const { seen } = await launch(h);
  assert.equal(seen.TOKEN_SHA, sha16(TOKEN), 'the launcher reads what the setter wrote');
  await assert.rejects(run('bash', [SETTER, '--url', 'https://bedrock.example', '--no-restart'], { env }), /exactly one of/);
  await assert.rejects(run('bash', [SETTER, '--url', 'https://bedrock.example/x', '--token-file', tokenFile, '--no-restart'], { env }), /https origin/);
});

test('set-ocean-federation.sh --keychain writes only a reference, and --off removes the file', async () => {
  const h = await harness();
  const env = { ...process.env, HOME: h.home, OCEAN_FEDERATION_ENV_FILE: h.envFile };
  await run('bash', [SETTER, '--url', 'https://bedrock.example', '--keychain', 'ocean-federation', '--no-restart'], { env });
  const body = await readFile(h.envFile, 'utf8');
  assert.match(body, /^OCEAN_FEDERATION_OWNER_TOKEN_KEYCHAIN=ocean-federation$/m);
  assert.ok(!/^OCEAN_FEDERATION_OWNER_TOKEN=/m.test(body), 'no bearer in the file');
  const { stdout } = await run('bash', [SETTER, '--off', '--no-restart'], { env });
  assert.match(stdout, /removed .*federation\.env/);
  const { seen } = await launch(h);
  assert.equal(seen.URL_SET, '');
});
