#!/usr/bin/env node
// Pre-render every status phrase into the WAV cache (one pass, no throttle).
import { readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const PKG_DIR = path.resolve(HERE, '..');
const file = process.env.OCEAN_VOICE_STATUS_PHRASES || path.join(PKG_DIR, 'config', 'status-phrases.json');
const render = path.join(PKG_DIR, 'tts', 'render-status-phrase.sh');

const phrases = JSON.parse(readFileSync(file, 'utf8'));
const all = new Set();
for (const phrase of phrases.thinking || []) all.add(phrase);
for (const list of Object.values(phrases.tools || {})) {
  for (const phrase of list || []) all.add(phrase);
}
for (const phrase of phrases.fallback || []) all.add(phrase);

let i = 0;
for (const phrase of all) {
  i++;
  console.error(`[${i}/${all.size}] ${phrase}`);
  const result = spawnSync(render, [phrase], { stdio: 'inherit' });
  if (result.status !== 0) process.exit(result.status ?? 1);
}
