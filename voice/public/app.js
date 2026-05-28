const els = {
  authPanel: document.querySelector('#auth-panel'),
  tokenInput: document.querySelector('#token-input'),
  saveToken: document.querySelector('#save-token'),
  statusDot: document.querySelector('#status-dot'),
  statusText: document.querySelector('#status-text'),
  micButton: document.querySelector('#mic-button'),
  micLabel: document.querySelector('#mic-label'),
  micHint: document.querySelector('#mic-hint'),
  prompt: document.querySelector('#prompt'),
  send: document.querySelector('#send'),
  clear: document.querySelector('#clear'),
  autoSend: document.querySelector('#auto-send'),
  speakHere: document.querySelector('#speak-here'),
  stopSpeech: document.querySelector('#stop-speech'),
  response: document.querySelector('#response'),
  debug: document.querySelector('#debug')
};

const state = {
  config: null,
  token: localStorage.getItem('oceanVoiceToken') || '',
  recorder: null,
  stream: null,
  chunks: [],
  recording: false,
  audio: null,
  audioUrl: null
};

els.saveToken.addEventListener('click', () => {
  state.token = els.tokenInput.value.trim();
  if (state.token) localStorage.setItem('oceanVoiceToken', state.token);
  refreshHealth();
});
els.micButton.addEventListener('click', () => state.recording ? stopRecording() : startRecording());
els.send.addEventListener('click', sendPrompt);
els.clear.addEventListener('click', () => {
  els.prompt.value = '';
  els.response.textContent = 'Ready.';
});
els.stopSpeech.addEventListener('click', stopSpeech);

init();

function loadTokenFromHash() {
  const hash = new URLSearchParams(window.location.hash.replace(/^#/, ''));
  const token = hash.get('token');
  if (!token) return;
  state.token = token.trim();
  localStorage.setItem('oceanVoiceToken', state.token);
  history.replaceState(null, '', window.location.pathname + window.location.search);
}

async function init() {
  loadTokenFromHash();
  if ('serviceWorker' in navigator) {
    navigator.serviceWorker.register('/sw.js').catch(() => {});
  }

  if (!window.isSecureContext) {
    els.micHint.textContent = 'Microphone capture needs localhost or HTTPS. Use Tailscale Serve or another HTTPS wrapper for phone access.';
  }

  try {
    state.config = await fetchJson('/api/config', { skipAuth: true });
    els.debug.textContent = JSON.stringify(state.config, null, 2);
    if (state.config.authRequired && !state.token) {
      els.authPanel.classList.remove('hidden');
      setStatus('busy', 'Token needed');
      return;
    }
    await refreshHealth();
  } catch (error) {
    setStatus('error', 'Connector offline');
    els.debug.textContent = String(error.message || error);
  }
}

async function refreshHealth() {
  try {
    const health = await fetchJson('/api/health');
    els.authPanel.classList.add('hidden');
    setStatus(health.oceanDaemon?.ok ? 'live' : 'busy', health.oceanDaemon?.ok ? 'Connected to Ocean' : 'Ocean unreachable');
    els.debug.textContent = JSON.stringify(health, null, 2);
  } catch (error) {
    if (String(error.message).includes('401')) {
      els.authPanel.classList.remove('hidden');
      setStatus('busy', 'Token needed');
    } else {
      setStatus('error', 'Voice agent offline');
    }
    els.debug.textContent = String(error.message || error);
  }
}

function setStatus(kind, text) {
  els.statusDot.className = `dot ${kind}`;
  els.statusText.textContent = text;
}

function pickMimeType() {
  const candidates = ['audio/webm;codecs=opus', 'audio/webm', 'audio/mp4', 'audio/ogg;codecs=opus'];
  for (const candidate of candidates) {
    if (window.MediaRecorder?.isTypeSupported?.(candidate)) return candidate;
  }
  return '';
}

async function startRecording() {
  if (!navigator.mediaDevices?.getUserMedia || !window.MediaRecorder) {
    els.response.textContent = 'This browser cannot record audio here. Type the prompt instead.';
    return;
  }

  try {
    stopSpeech();
    state.stream = await navigator.mediaDevices.getUserMedia({
      audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true }
    });
    state.chunks = [];
    const mimeType = pickMimeType();
    state.recorder = new MediaRecorder(state.stream, mimeType ? { mimeType } : undefined);
    state.recorder.ondataavailable = (event) => {
      if (event.data?.size) state.chunks.push(event.data);
    };
    state.recorder.onstop = onRecordingStopped;
    state.recorder.start();
    state.recording = true;
    els.micButton.classList.add('recording');
    els.micButton.setAttribute('aria-pressed', 'true');
    els.micLabel.textContent = 'Listening… tap to stop';
    setStatus('busy', 'Recording');
  } catch (error) {
    els.response.textContent = `Microphone error: ${error.message || error}`;
    setStatus('error', 'Mic blocked');
  }
}

function stopRecording() {
  if (!state.recorder || !state.recording) return;
  state.recording = false;
  els.micButton.classList.remove('recording');
  els.micButton.setAttribute('aria-pressed', 'false');
  els.micLabel.textContent = 'Transcribing…';
  setStatus('busy', 'Transcribing');
  state.recorder.stop();
  state.stream?.getTracks?.().forEach((track) => track.stop());
}

async function onRecordingStopped() {
  try {
    const type = state.recorder?.mimeType || 'audio/webm';
    const blob = new Blob(state.chunks, { type });
    if (blob.size < 800) {
      els.response.textContent = 'That recording was too short. Try again or type the prompt.';
      return;
    }
    const transcript = await transcribe(blob);
    if (!transcript) {
      els.response.textContent = 'I could not hear a transcript. Try again.';
      return;
    }
    els.prompt.value = transcript;
    els.response.textContent = `Heard: ${transcript}`;
    if (els.autoSend.checked) await sendPrompt();
  } catch (error) {
    els.response.textContent = `Transcription error: ${error.message || error}`;
    setStatus('error', 'STT failed');
  } finally {
    els.micLabel.textContent = 'Tap to talk';
    state.recorder = null;
    state.stream = null;
    state.chunks = [];
  }
}

async function transcribe(blob) {
  const ext = blob.type.includes('mp4') ? 'm4a' : blob.type.includes('ogg') ? 'ogg' : 'webm';
  const data = await fetchJson(`/api/stt?filename=clip.${ext}`, {
    method: 'POST',
    headers: { 'content-type': blob.type || 'application/octet-stream' },
    body: blob,
    rawBody: true
  });
  return (data.text || '').trim();
}

async function sendPrompt() {
  const prompt = els.prompt.value.trim();
  if (!prompt) return;
  stopSpeech();
  setStatus('busy', 'Ocean thinking');
  els.send.disabled = true;
  els.micButton.disabled = true;
  els.response.textContent = 'Sending to Ocean…';
  try {
    const result = await fetchJson('/api/prompt', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ prompt })
    });
    const text = result.text || result.response || result.message || JSON.stringify(result, null, 2);
    els.response.textContent = text;
    setStatus('live', 'Connected');
    if (els.speakHere.checked) void speak(text);
    void refreshHealth();
  } catch (error) {
    els.response.textContent = `Prompt error: ${error.message || error}`;
    setStatus('error', 'Prompt failed');
  } finally {
    els.send.disabled = false;
    els.micButton.disabled = false;
  }
}

async function speak(text) {
  stopSpeech();
  try {
    const response = await fetch('/api/tts', {
      method: 'POST',
      headers: { ...authHeaders(), 'content-type': 'application/json' },
      body: JSON.stringify({ text })
    });
    if (!response.ok) throw new Error(`server TTS ${response.status}`);
    const blob = await response.blob();
    state.audioUrl = URL.createObjectURL(blob);
    state.audio = new Audio(state.audioUrl);
    state.audio.addEventListener('ended', cleanupAudio, { once: true });
    await state.audio.play();
    return;
  } catch (error) {
    console.warn('server TTS failed, falling back to browser speech', error);
  }

  if (!('speechSynthesis' in window)) return;
  const utterance = new SpeechSynthesisUtterance(text);
  utterance.rate = 1.02;
  utterance.pitch = 1;
  window.speechSynthesis.speak(utterance);
}

function stopSpeech() {
  window.speechSynthesis?.cancel?.();
  if (state.audio) {
    try { state.audio.pause(); } catch {}
    state.audio = null;
  }
  cleanupAudio();
}

function cleanupAudio() {
  if (state.audioUrl) {
    URL.revokeObjectURL(state.audioUrl);
    state.audioUrl = null;
  }
}

function authHeaders() {
  return state.token ? { authorization: `Bearer ${state.token}` } : {};
}

async function fetchJson(url, options = {}) {
  const headers = { ...(options.skipAuth ? {} : authHeaders()), ...(options.headers || {}) };
  const response = await fetch(url, { ...options, headers });
  const text = await response.text();
  let data;
  try { data = text ? JSON.parse(text) : {}; }
  catch { data = { text }; }
  if (!response.ok) {
    throw new Error(`${response.status} ${data.error || response.statusText}`);
  }
  return data;
}
