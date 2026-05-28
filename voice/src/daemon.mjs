#!/usr/bin/env node
//
// ocean-voice daemon — the local/desktop voice surface for the Ocean runtime.
//
// This is a THIN CLIENT. It captures a prompt over a tiny HTTP API, hands it to
// the Rust ocean-daemon via ocean-client.mjs, speaks status phrases while the
// runtime works, and speaks the final answer through TTS. It holds no agent
// logic, no provider keys, and no sessions of its own — the Rust runtime owns
// all of that. (The previous third-party "pi" backend has been removed.)

import http from 'node:http';
import crypto from 'node:crypto';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { existsSync, readFileSync } from 'node:fs';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { spawn, spawnSync } from 'node:child_process';

import { runOceanTurn, oceanHealth, OCEAN_DAEMON_URL } from './ocean-client.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url)); // voice/src
const PKG_DIR = path.resolve(HERE, '..'); // voice/
const HOME = process.env.HOME || os.homedir();
const UID = typeof process.getuid === 'function' ? process.getuid() : 1000;

// --- portable config (env overrides, sensible defaults) ---
const TTS_DIR = process.env.OCEAN_VOICE_TTS_DIR || path.join(PKG_DIR, 'tts');
const CONFIG_DIR = process.env.OCEAN_VOICE_CONFIG_DIR || path.join(PKG_DIR, 'config');
const STATE_DIR = process.env.OCEAN_VOICE_STATE_DIR || path.join(HOME, '.ocean-voice');
const STATUS_AUDIO_DIR = process.env.OCEAN_VOICE_STATUS_AUDIO_DIR || path.join(STATE_DIR, 'status-audio');
const CWD = process.env.OCEAN_VOICE_CWD || HOME; // working dir for Ocean turns

const STATE_FILE = path.join(STATE_DIR, 'state.json');
const RESPONSE_FILE = path.join(STATE_DIR, 'last-response.txt');
const SPOKEN_FILE = path.join(STATE_DIR, 'last-spoken.txt');
const STATUS_SPOKEN_FILE = path.join(STATE_DIR, 'status-spoken.txt');
const INSTRUCTIONS_FILE = path.join(CONFIG_DIR, 'voice-agent-instructions.md');
const STATUS_PHRASES_FILE = path.join(CONFIG_DIR, 'status-phrases.json');
const SPEAK = path.join(TTS_DIR, 'speak-response.sh');
const SPEAK_STATUS = path.join(TTS_DIR, 'speak-status.sh');

const HOST = process.env.OCEAN_VOICE_HOST || process.env.PI_VOICE_AGENT_HOST || '127.0.0.1';
const PORT = Number(process.env.OCEAN_VOICE_PORT || process.env.PI_VOICE_AGENT_PORT || 8787);
const OCEAN_STATUS_FIRST_DELAY_MS = Number(process.env.OCEAN_STATUS_FIRST_DELAY_MS || 1_800);
const OCEAN_STATUS_INTERVAL_MS = Number(process.env.OCEAN_STATUS_INTERVAL_MS || 6_500);

process.env.HOME = HOME;
process.env.XDG_RUNTIME_DIR ||= `/run/user/${UID}`;
process.env.PULSE_SERVER ||= `unix:/run/user/${UID}/pulse/native`;

await mkdir(STATE_DIR, { recursive: true });
await mkdir(STATUS_AUDIO_DIR, { recursive: true });

let oceanSessionId = null;
let busy = false;
let localSpeechEnabled = true;
let speechChild = null;
let statusSpeechChild = null;
let lastStatusSpokenAt = 0;
let statusTimer = null;
let oceanStatusTimer = null;
let oceanStatusInterval = null;

// Desktop on-screen-display status (best-effort; no-op if the helper is absent).
function setStatus(status) {
  spawnSync(`${HOME}/.local/bin/dictate-status`, [status], {
    stdio: 'ignore',
    env: {
      ...process.env,
      XDG_RUNTIME_DIR: `/run/user/${UID}`,
      DBUS_SESSION_BUS_ADDRESS: `unix:path=/run/user/${UID}/bus`,
      DISPLAY: process.env.DISPLAY || ':0',
      XAUTHORITY: process.env.XAUTHORITY || `${HOME}/.Xauthority`,
    },
  });
}

async function loadState() {
  try {
    return JSON.parse(await readFile(STATE_FILE, 'utf8'));
  } catch {
    return {};
  }
}

async function saveState(extra = {}) {
  await writeFile(STATE_FILE, JSON.stringify({
    backend: 'ocean',
    sessionId: oceanSessionId,
    oceanSessionId,
    oceanDaemonUrl: OCEAN_DAEMON_URL,
    updatedAt: new Date().toISOString(),
    ...extra,
  }, null, 2));
}

async function createSession({ fresh = false } = {}) {
  const state = fresh ? {} : await loadState();
  oceanSessionId = state.oceanSessionId || state.sessionId || null;
  await saveState({ oceanSessionId });
  console.error(`[ocean-voice] ready — daemon ${OCEAN_DAEMON_URL}, session ${oceanSessionId ?? 'new'}`);
}

function spokenVersion(text) {
  const clean = (text.trim() || 'Done.')
    .replace(/`([^`]+)`/g, '$1')
    .replace(/\b\S+\.(?:md|rs|js|mjs|ts|tsx|json|toml|yaml|yml|txt|sh|py)\b/gi, 'that file')
    .replace(/\b(?:\/?[\w.-]+\/){2,}[\w.-]+\b/g, 'that path')
    .replace(/https?:\/\/\S+/gi, 'that link')
    .replace(/\s+/g, ' ');
  if (clean.length <= 420) return clean;
  return `${clean.slice(0, 390).trim()}… Full details are saved in the response file.`;
}

function killStatusSpeech() {
  if (statusTimer) {
    clearTimeout(statusTimer);
    statusTimer = null;
  }
  if (statusSpeechChild) {
    try { process.kill(-statusSpeechChild.pid, 'SIGTERM'); } catch {}
    statusSpeechChild = null;
  }
}

function spawnSpeech(file, { status = false, text = null } = {}) {
  const command = status ? SPEAK_STATUS : SPEAK;
  const args = status ? [text] : [file];
  const child = spawn(command, args, { stdio: 'ignore', env: process.env, detached: true });
  if (status) statusSpeechChild = child;
  else speechChild = child;

  child.on('close', () => {
    if (status && statusSpeechChild === child) statusSpeechChild = null;
    if (!status && speechChild === child) {
      speechChild = null;
      setStatus('');
    }
  });
  child.on('error', (error) => {
    console.error(status ? '[ocean-voice status speech error]' : '[ocean-voice speak error]', error);
    if (status && statusSpeechChild === child) statusSpeechChild = null;
    if (!status && speechChild === child) {
      speechChild = null;
      setStatus('');
    }
  });
}

function loadStatusPhrases() {
  try {
    return JSON.parse(readFileSync(STATUS_PHRASES_FILE, 'utf8'));
  } catch {
    return {};
  }
}

function statusAudioPath(text) {
  const hash = crypto.createHash('sha1').update(text).digest('hex');
  return path.join(STATUS_AUDIO_DIR, `${hash}.wav`);
}

function pickPhrase(list, fallback = 'Let me check that.') {
  if (!Array.isArray(list) || list.length === 0) return fallback;
  const cached = list.filter((phrase) => existsSync(statusAudioPath(phrase)));
  const source = cached.length > 0 ? cached : list;
  return source[Math.floor(Math.random() * source.length)] || fallback;
}

async function speakStatus(text, delayMs = 450, { force = false } = {}) {
  if (!localSpeechEnabled) return;
  const now = Date.now();
  if (!force && now - lastStatusSpokenAt < 2500) return;
  if (speechChild || statusSpeechChild) return;
  if (statusTimer) {
    clearTimeout(statusTimer);
    statusTimer = null;
  }
  statusTimer = setTimeout(async () => {
    statusTimer = null;
    if (!busy || speechChild || statusSpeechChild) return;
    lastStatusSpokenAt = Date.now();
    await writeFile(STATUS_SPOKEN_FILE, `${text}\n`);
    console.error(`[ocean-voice status-speech] ${text}`);
    spawnSpeech(STATUS_SPOKEN_FILE, { status: true, text });
  }, delayMs);
}

function speakStatusForTool(toolName, options = {}) {
  const phrases = loadStatusPhrases();
  const phrase = pickPhrase(phrases.tools?.[toolName], pickPhrase(phrases.fallback));
  void speakStatus(phrase, 0, options);
}

function normalizeToolName(toolName) {
  const name = String(toolName || '').toLowerCase();
  if (name.includes('bash') || name.includes('shell')) return 'bash';
  if (name.includes('read')) return 'read';
  if (name.includes('write')) return 'write';
  if (name.includes('edit')) return 'edit';
  if (name.includes('grep') || name.includes('search')) return 'grep';
  if (name.includes('find') || name.includes('glob')) return 'find';
  if (name === 'ls' || name.includes('list')) return 'ls';
  return name;
}

function pickOceanWaitingPhrase() {
  const phrases = loadStatusPhrases();
  const waiting = [
    ...(Array.isArray(phrases.fallback) ? phrases.fallback : []),
    ...(Array.isArray(phrases.thinking) ? phrases.thinking : []),
  ];
  return pickPhrase(waiting, 'Still working on that.');
}

function stopOceanWaitingStatus() {
  if (oceanStatusTimer) {
    clearTimeout(oceanStatusTimer);
    oceanStatusTimer = null;
  }
  if (oceanStatusInterval) {
    clearInterval(oceanStatusInterval);
    oceanStatusInterval = null;
  }
}

function startOceanWaitingStatus() {
  stopOceanWaitingStatus();
  const say = () => {
    if (!busy) return;
    const phrase = pickOceanWaitingPhrase();
    setStatus(phrase);
    void speakStatus(phrase, 0, { force: true });
  };
  oceanStatusTimer = setTimeout(() => {
    oceanStatusTimer = null;
    say();
    oceanStatusInterval = setInterval(say, OCEAN_STATUS_INTERVAL_MS);
  }, OCEAN_STATUS_FIRST_DELAY_MS);
}

async function speak(text) {
  killStatusSpeech();
  const fullText = text.trim() || 'Done.';
  await writeFile(RESPONSE_FILE, `${fullText}\n`);
  await writeFile(SPOKEN_FILE, `${spokenVersion(fullText)}\n`);

  // Web/PWA callers play audio on their own device, so the desktop stays silent for them.
  if (!localSpeechEnabled) return;

  setStatus('🔊 SPEAKING');
  spawnSpeech(SPOKEN_FILE);
}

function buildOceanVoicePrompt(prompt) {
  const custom = existsSync(INSTRUCTIONS_FILE) ? readFileSync(INSTRUCTIONS_FILE, 'utf8').trim() : '';
  return `[Voice channel invocation for Mozart]
This request came through the local ocean-voice surface on the user's Linux workstation.
You are the Ocean OS voice agent and local computer-use expert.
The user hears your final answer through text-to-speech, so optimise the final answer for spoken delivery.
Be a little more explanatory than a terse one-liner: for non-trivial answers, give three to five short useful sentences.
Do safe inspections directly. Ask before installs, credentials, network exposure, deletes, commits, pushes, or broad refactors.
When speaking, prefer friendly names and say Ocean Current instead of daemon unless naming an exact internal service.
Do not end with vague "if you want" filler; give a concrete next step or take the safe next step.
${custom ? `\nVoice profile instructions:\n${custom}\n` : ''}
User said:
${prompt}`;
}

function handleAgentEvent(evt) {
  if (evt.type === 'tool_call_started') {
    const tool = normalizeToolName(evt.call?.name || evt.call?.tool || '');
    console.error(`[ocean-voice tool] ${tool}`);
    setStatus(`tool: ${tool}`);
    killStatusSpeech();
    speakStatusForTool(tool, { force: true });
  }
}

async function handleOceanPrompt(prompt) {
  console.error(`[ocean-voice prompt] ${prompt}`);
  setStatus('🌊 OCEAN WORKING');
  startOceanWaitingStatus();
  try {
    const result = await runOceanTurn({
      prompt: buildOceanVoicePrompt(prompt),
      cwd: CWD,
      sessionId: oceanSessionId,
      onEvent: handleAgentEvent,
    });
    oceanSessionId = result.sessionId || oceanSessionId;
    await saveState({ oceanSessionId });
    const text = result.text || result.error || 'Done.';
    await speak(text);
    return { ok: result.ok, text, backend: 'ocean', sessionId: oceanSessionId, turnId: result.turnId };
  } finally {
    stopOceanWaitingStatus();
  }
}

async function handlePrompt(prompt, { speakLocal = true } = {}) {
  localSpeechEnabled = speakLocal;
  prompt = prompt.trim();
  if (!prompt) return { ok: true, text: 'Empty prompt ignored.' };

  if (/^(new|reset|start over) voice session\.?$/i.test(prompt)) {
    oceanSessionId = null;
    await saveState({ oceanSessionId: null });
    const text = 'Started a fresh Ocean voice session.';
    await speak(text);
    return { ok: true, text, backend: 'ocean', sessionId: null };
  }

  return handleOceanPrompt(prompt);
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on('data', (chunk) => chunks.push(chunk));
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    req.on('error', reject);
  });
}

await createSession();
setStatus('');

const startupHealth = await oceanHealth();
if (!startupHealth.ok) {
  console.error(`[ocean-voice] WARNING: ocean-daemon not reachable at ${OCEAN_DAEMON_URL} (${startupHealth.error || startupHealth.status}). Prompts will fail until it is up.`);
}

const server = http.createServer(async (req, res) => {
  try {
    if (req.method === 'GET' && req.url === '/health') {
      const daemon = await oceanHealth();
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({
        ok: true,
        busy,
        backend: 'ocean',
        sessionId: oceanSessionId,
        oceanDaemonUrl: OCEAN_DAEMON_URL,
        oceanDaemonReachable: Boolean(daemon.ok),
      }));
      return;
    }

    if (req.method === 'POST' && req.url === '/prompt') {
      if (busy) {
        res.writeHead(409, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ ok: false, error: 'voice agent is busy' }));
        return;
      }
      busy = true;
      const body = await readBody(req);
      let prompt = body;
      let speakLocal = true;
      const contentType = req.headers['content-type'] || '';
      if (contentType.includes('application/json')) {
        const parsed = JSON.parse(body);
        prompt = parsed.prompt || '';
        // Callers (e.g. the web connector) set speak:false so audio is delivered to their device.
        if (parsed.speak === false) speakLocal = false;
      }
      const result = await handlePrompt(prompt, { speakLocal });
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify(result));
      busy = false;
      return;
    }

    res.writeHead(404, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ ok: false, error: 'not found' }));
  } catch (error) {
    busy = false;
    setStatus('');
    console.error('[ocean-voice error]', error);
    res.writeHead(500, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ ok: false, error: String(error?.stack || error) }));
  }
});

server.listen(PORT, HOST, () => {
  console.error(`[ocean-voice] listening http://${HOST}:${PORT} → ocean-daemon ${OCEAN_DAEMON_URL}`);
});

process.on('SIGTERM', () => {
  server.close(() => process.exit(0));
});
