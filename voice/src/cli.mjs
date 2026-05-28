#!/usr/bin/env node
//
// ocean-voice CLI — one-shot. Send a single prompt to the Ocean runtime and
// print the assistant's reply. A thin client over ocean-client.mjs; holds no
// agent logic or credentials of its own.
//
//   node cli.mjs "summarise the ocean roadmap"
//   echo "what changed today?" | node cli.mjs

import os from 'node:os';
import { runOceanTurn, OCEAN_DAEMON_URL } from './ocean-client.mjs';

const cwd = process.env.OCEAN_VOICE_CWD || process.env.HOME || os.homedir();

async function readStdin() {
  if (process.stdin.isTTY) return '';
  const chunks = [];
  for await (const chunk of process.stdin) chunks.push(chunk);
  return Buffer.concat(chunks).toString('utf8');
}

const prompt = process.argv.slice(2).join(' ').trim() || (await readStdin()).trim();
if (!prompt) {
  console.error('usage: ocean-voice <prompt>   (or pipe text on stdin)');
  console.error(`talks to ocean-daemon at ${OCEAN_DAEMON_URL} (set OCEAN_DAEMON_URL to change)`);
  process.exit(2);
}

try {
  const result = await runOceanTurn({ prompt, cwd });
  process.stdout.write(`${result.text || result.error || 'Done.'}\n`);
  process.exit(result.ok ? 0 : 1);
} catch (error) {
  console.error(`[ocean-voice] ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
}
