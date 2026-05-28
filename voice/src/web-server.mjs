#!/usr/bin/env node
import http from 'node:http';
import { timingSafeEqual } from 'node:crypto';
import { createReadStream, existsSync, readFileSync } from 'node:fs';
import { mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { homedir, tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { runOceanTurn, oceanHealth, OCEAN_DAEMON_URL } from './ocean-client.mjs';

const __filename = fileURLToPath(import.meta.url);
const APP_ROOT = path.dirname(__filename); // voice/src
const PKG_DIR = path.resolve(APP_ROOT, '..'); // voice/
const PUBLIC_ROOT = path.join(PKG_DIR, 'public');
const HOME = process.env.HOME || homedir();

const HOST = process.env.VOICE_WEB_HOST || '127.0.0.1';
const PORT = Number(process.env.VOICE_WEB_PORT || 8790);
const TOKEN = process.env.VOICE_WEB_TOKEN || '';
const BASIC_USER = process.env.VOICE_WEB_USER || '';
const BASIC_PASS = process.env.VOICE_WEB_PASSWORD || '';
const BASIC_ENABLED = Boolean(BASIC_USER && BASIC_PASS);
const AUTH_ENABLED = Boolean(TOKEN) || BASIC_ENABLED;
const PROMPT_TIMEOUT_MS = Number(process.env.VOICE_WEB_PROMPT_TIMEOUT_MS || 300_000);
const STT_TIMEOUT_MS = Number(process.env.VOICE_WEB_STT_TIMEOUT_MS || 45_000);
const MAX_AUDIO_BYTES = Number(process.env.VOICE_WEB_MAX_AUDIO_BYTES || 25 * 1024 * 1024);
const XAI_STT_MODEL = process.env.XAI_STT_MODEL || 'grok-stt';
const XAI_TTS_SCRIPT = process.env.XAI_TTS_SCRIPT || path.join(PKG_DIR, 'tts', 'xai-tts-wav.sh');
const XAI_SETTINGS_FILE = process.env.XAI_SETTINGS_FILE || path.join(HOME, '.pi/agent/settings.json');
const MAX_TTS_CHARS = Number(process.env.VOICE_WEB_MAX_TTS_CHARS || 4_000);
const OCEAN_VOICE_CWD = process.env.OCEAN_VOICE_CWD || HOME;

// Session continuity for this connector instance (the Rust runtime owns the session).
let webSessionId = null;
// Single-flight guard: one Ocean turn at a time through this connector.
let webBusy = false;

const LOCAL_BINDS = new Set(['127.0.0.1', 'localhost', '::1']);
if (!LOCAL_BINDS.has(HOST) && !AUTH_ENABLED) {
  console.error('Refusing to bind beyond localhost without auth.');
  console.error('Set VOICE_WEB_TOKEN, or VOICE_WEB_USER + VOICE_WEB_PASSWORD, before using a LAN/Tailscale address.');
  process.exit(1);
}

const MIME_TYPES = {
  '.css': 'text/css; charset=utf-8',
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.map': 'application/json; charset=utf-8',
  '.png': 'image/png',
  '.svg': 'image/svg+xml',
  '.txt': 'text/plain; charset=utf-8',
  '.webmanifest': 'application/manifest+json; charset=utf-8'
};

function json(res, status, data) {
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'cache-control': 'no-store'
  });
  res.end(JSON.stringify(data));
}

function safeEqual(a, b) {
  const ab = Buffer.from(String(a));
  const bb = Buffer.from(String(b));
  if (ab.length !== bb.length) return false;
  return timingSafeEqual(ab, bb);
}

function isAuthorized(req) {
  if (!AUTH_ENABLED) return true;
  const auth = req.headers.authorization || '';
  if (TOKEN && auth === `Bearer ${TOKEN}`) return true;
  if (BASIC_ENABLED && auth.startsWith('Basic ')) {
    const decoded = Buffer.from(auth.slice(6), 'base64').toString('utf8');
    const idx = decoded.indexOf(':');
    if (idx < 0) return false;
    const user = decoded.slice(0, idx);
    const pass = decoded.slice(idx + 1);
    if (safeEqual(user, BASIC_USER) && safeEqual(pass, BASIC_PASS)) return true;
  }
  return false;
}

function challenge(res) {
  const headers = {
    'content-type': 'application/json; charset=utf-8',
    'cache-control': 'no-store'
  };
  if (BASIC_ENABLED) headers['www-authenticate'] = 'Basic realm="ocean-voice", charset="UTF-8"';
  res.writeHead(401, headers);
  res.end(JSON.stringify({ ok: false, error: 'unauthorized', authRequired: AUTH_ENABLED }));
}

function requireAuth(req, res) {
  if (isAuthorized(req)) return true;
  challenge(res);
  return false;
}

function readBody(req, maxBytes = 1024 * 1024) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on('data', (chunk) => {
      size += chunk.length;
      if (size > maxBytes) {
        reject(new Error(`request body too large; max ${maxBytes} bytes`));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on('end', () => resolve(Buffer.concat(chunks)));
    req.on('error', reject);
  });
}

async function readJson(req) {
  const body = await readBody(req);
  if (!body.length) return {};
  return JSON.parse(body.toString('utf8'));
}

function timeoutSignal(ms) {
  const ac = new AbortController();
  const timer = setTimeout(() => ac.abort(new Error(`timeout after ${ms}ms`)), ms);
  return { signal: ac.signal, clear: () => clearTimeout(timer) };
}

function getXaiApiKey() {
  if (process.env.XAI_API_KEY) return process.env.XAI_API_KEY;
  try {
    if (!existsSync(XAI_SETTINGS_FILE)) return '';
    const settings = JSON.parse(readFileSync(XAI_SETTINGS_FILE, 'utf8'));
    return settings?.xai?.apiKey || '';
  } catch {
    return '';
  }
}

async function transcribeAudio(req, requestUrl) {
  const apiKey = getXaiApiKey();
  if (!apiKey) {
    return { status: 500, data: { ok: false, error: 'XAI_API_KEY is not configured for STT.' } };
  }

  const startedAt = Date.now();
  const body = await readBody(req, MAX_AUDIO_BYTES);
  if (!body.length) {
    return { status: 400, data: { ok: false, error: 'empty audio body' } };
  }

  const contentType = req.headers['content-type'] || 'application/octet-stream';
  const filename = safeFilename(requestUrl.searchParams.get('filename') || filenameForContentType(contentType));
  const fd = new FormData();
  fd.append('file', new Blob([body], { type: contentType }), filename);
  fd.append('model', XAI_STT_MODEL);
  fd.append('language', 'en');
  fd.append('response_format', 'json');

  const timer = timeoutSignal(STT_TIMEOUT_MS);
  try {
    const response = await fetch('https://api.x.ai/v1/stt', {
      method: 'POST',
      headers: { Authorization: `Bearer ${apiKey}` },
      body: fd,
      signal: timer.signal
    });
    const raw = await response.text();
    let data;
    try {
      data = raw ? JSON.parse(raw) : {};
    } catch {
      data = { raw };
    }
    if (!response.ok) {
      return { status: response.status, data: { ok: false, error: 'stt_failed', detail: data } };
    }
    return {
      status: 200,
      data: {
        ok: true,
        text: String(data.text || '').trim(),
        ms: Date.now() - startedAt,
        model: XAI_STT_MODEL
      }
    };
  } finally {
    timer.clear();
  }
}

function safeFilename(name) {
  return String(name || 'clip.webm').replace(/[^a-zA-Z0-9._-]/g, '_').slice(0, 80) || 'clip.webm';
}

function filenameForContentType(contentType) {
  if (contentType.includes('mp4') || contentType.includes('m4a')) return 'clip.m4a';
  if (contentType.includes('ogg')) return 'clip.ogg';
  if (contentType.includes('wav')) return 'clip.wav';
  return 'clip.webm';
}

async function synthesizeSpeech(text) {
  if (!existsSync(XAI_TTS_SCRIPT)) {
    return { ok: false, status: 500, error: 'tts script missing' };
  }
  const dir = await mkdtemp(path.join(tmpdir(), 'voice-web-tts-'));
  const textPath = path.join(dir, 'text.txt');
  const wavPath = path.join(dir, 'speech.wav');
  try {
    await writeFile(textPath, `${text}\n`);
    await runCommand(XAI_TTS_SCRIPT, [textPath, wavPath], 75_000);
    const audio = await readFile(wavPath);
    return { ok: true, audio };
  } catch (error) {
    return { ok: false, status: 500, error: error instanceof Error ? error.message : String(error) };
  } finally {
    await rm(dir, { recursive: true, force: true }).catch(() => {});
  }
}

function runCommand(command, args, timeoutMs) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: ['ignore', 'ignore', 'pipe'], env: process.env });
    let stderr = '';
    const timer = setTimeout(() => {
      try { child.kill('SIGTERM'); } catch {}
      reject(new Error(`${path.basename(command)} timed out`));
    }, timeoutMs);
    child.stderr.on('data', (chunk) => { stderr += chunk.toString(); });
    child.on('error', (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.on('close', (code) => {
      clearTimeout(timer);
      if (code === 0) resolve();
      else reject(new Error(`${path.basename(command)} exited ${code}: ${stderr.slice(-1000)}`));
    });
  });
}

async function handleApi(req, res, requestUrl) {
  if (req.method === 'GET' && requestUrl.pathname === '/api/config') {
    return json(res, 200, {
      ok: true,
      name: 'ocean-voice web',
      authRequired: AUTH_ENABLED,
      basicAuth: BASIC_ENABLED,
      oceanDaemonUrl: OCEAN_DAEMON_URL,
      sttEnabled: Boolean(getXaiApiKey()),
      ttsEnabled: existsSync(XAI_TTS_SCRIPT) && Boolean(getXaiApiKey()),
      localOnly: LOCAL_BINDS.has(HOST)
    });
  }

  if (!requireAuth(req, res)) return;

  if (req.method === 'GET' && requestUrl.pathname === '/api/health') {
    const daemon = await oceanHealth();
    return json(res, daemon.ok ? 200 : 502, {
      ok: Boolean(daemon.ok),
      connector: true,
      oceanDaemonUrl: OCEAN_DAEMON_URL,
      oceanDaemon: daemon
    });
  }

  if (req.method === 'POST' && requestUrl.pathname === '/api/prompt') {
    const body = await readJson(req);
    const prompt = String(body.prompt || '').trim();
    if (!prompt) return json(res, 400, { ok: false, error: 'prompt required' });
    // "new voice session" resets continuity; otherwise continue the connector's session.
    if (/^(new|reset|start over) voice session\.?$/i.test(prompt)) {
      webSessionId = null;
      return json(res, 200, { ok: true, text: 'Started a fresh Ocean voice session.', sessionId: null });
    }
    // Single-flight: runOceanTurn attributes every event in its window to the one
    // in-flight turn, and webSessionId must not be raced. Reject overlap (the
    // daemon surface has the same guard) rather than cross-contaminate replies.
    if (webBusy) {
      return json(res, 409, { ok: false, error: 'voice agent is busy' });
    }
    webBusy = true;
    try {
      const result = await runOceanTurn({
        prompt,
        cwd: OCEAN_VOICE_CWD,
        sessionId: webSessionId,
        timeoutMs: PROMPT_TIMEOUT_MS
      });
      webSessionId = result.sessionId || webSessionId;
      return json(res, 200, {
        ok: result.ok,
        text: result.text || result.error || 'Done.',
        sessionId: webSessionId,
        turnId: result.turnId,
        backend: 'ocean'
      });
    } catch (error) {
      return json(res, 502, { ok: false, error: error instanceof Error ? error.message : String(error) });
    } finally {
      webBusy = false;
    }
  }

  if (req.method === 'POST' && requestUrl.pathname === '/api/stt') {
    const result = await transcribeAudio(req, requestUrl);
    return json(res, result.status, result.data);
  }

  if (req.method === 'POST' && requestUrl.pathname === '/api/tts') {
    const body = await readJson(req);
    const text = String(body.text || '').trim().slice(0, MAX_TTS_CHARS);
    if (!text) return json(res, 400, { ok: false, error: 'text required' });
    const result = await synthesizeSpeech(text);
    if (!result.ok) return json(res, result.status || 500, result);
    res.writeHead(200, {
      'content-type': 'audio/wav',
      'cache-control': 'no-store',
      'content-length': result.audio.length,
    });
    res.end(result.audio);
    return;
  }

  return json(res, 404, { ok: false, error: 'not found' });
}

async function serveStatic(req, res, requestUrl) {
  let pathname = decodeURIComponent(requestUrl.pathname);
  if (pathname === '/') pathname = '/index.html';
  const target = path.normalize(path.join(PUBLIC_ROOT, pathname));
  if (!target.startsWith(PUBLIC_ROOT)) {
    res.writeHead(403, { 'content-type': 'text/plain; charset=utf-8' });
    res.end('Forbidden');
    return;
  }
  try {
    const info = await stat(target);
    if (!info.isFile()) throw new Error('not a file');
    const ext = path.extname(target).toLowerCase();
    res.writeHead(200, {
      'content-type': MIME_TYPES[ext] || 'application/octet-stream',
      'cache-control': ext === '.html' ? 'no-store' : 'public, max-age=3600'
    });
    createReadStream(target).pipe(res);
  } catch {
    const fallback = path.join(PUBLIC_ROOT, 'index.html');
    const html = await readFile(fallback, 'utf8');
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' });
    res.end(html);
  }
}

const server = http.createServer(async (req, res) => {
  try {
    const requestUrl = new URL(req.url || '/', `http://${req.headers.host || `${HOST}:${PORT}`}`);
    // With password login on, protect the whole site so the browser prompts for credentials on first load.
    if (BASIC_ENABLED && !isAuthorized(req)) {
      return challenge(res);
    }
    if (requestUrl.pathname.startsWith('/api/')) {
      return await handleApi(req, res, requestUrl);
    }
    if (req.method !== 'GET' && req.method !== 'HEAD') {
      return json(res, 405, { ok: false, error: 'method not allowed' });
    }
    return await serveStatic(req, res, requestUrl);
  } catch (error) {
    console.error('[voice-web-connector]', error);
    return json(res, 500, { ok: false, error: error instanceof Error ? error.message : String(error) });
  }
});

server.listen(PORT, HOST, () => {
  console.log(`ocean-voice web ready at http://${HOST}:${PORT}`);
  console.log(`Steering the Ocean runtime at ${OCEAN_DAEMON_URL}`);
  console.log(TOKEN ? 'API token auth is enabled.' : 'Localhost-only mode; no connector token required.');
});
