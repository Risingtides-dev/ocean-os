#!/usr/bin/env node
import { existsSync, readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const PKG_DIR = path.resolve(HERE, '..');
const STATE_DIR = process.env.OCEAN_VOICE_STATE_DIR || path.join(process.env.HOME || os.homedir(), '.ocean-voice');
const phrasesFile = process.env.OCEAN_VOICE_STATUS_PHRASES || path.join(PKG_DIR, 'config', 'status-phrases.json');
const cacheDir = process.env.OCEAN_VOICE_STATUS_AUDIO_DIR || path.join(STATE_DIR, 'status-audio');
const render = path.join(PKG_DIR, 'tts', 'render-status-phrase.sh');
const maxNew = Number(process.env.STATUS_CACHE_MAX_NEW || process.argv[2] || 5);
const pauseMs = Number(process.env.STATUS_CACHE_PAUSE_MS || 90000);

function sleep(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

function wavPath(text) {
  const hash = crypto.createHash('sha1').update(text).digest('hex');
  return `${cacheDir}/${hash}.wav`;
}

const phrases = JSON.parse(readFileSync(phrasesFile, 'utf8'));
const all = [];
for (const phrase of phrases.thinking || []) all.push(phrase);
for (const list of Object.values(phrases.tools || {})) {
  for (const phrase of list || []) all.push(phrase);
}
for (const phrase of phrases.fallback || []) all.push(phrase);

const unique = [...new Set(all)];
let rendered = 0;
for (const phrase of unique) {
  if (existsSync(wavPath(phrase))) continue;
  if (rendered >= maxNew) break;
  rendered++;
  console.error(`[slow-cache ${rendered}/${maxNew}] ${phrase}`);
  const result = spawnSync(render, [phrase], { stdio: 'inherit' });
  if (result.status !== 0) {
    console.error(`[slow-cache] failed: ${phrase}`);
  }
  if (rendered < maxNew) sleep(pauseMs);
}
console.error(`[slow-cache] rendered ${rendered}; cached total may be checked with find status-audio`);
