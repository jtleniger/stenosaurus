import { StreamingResampler, PcmAccumulator, AsyncQueue } from '/static/pcm.js';

const VENDOR = '/static/vendor/cactus-needle-0.3.0/dist/browser/index.js';
const MODEL_URL = '/model/whistle.cact';
const MODEL_SHA256 = 'b6e02f048568ac5d01a2042556c658061e699acbc0aa2a1439f52f3d461dffeb';

const recorderEl = document.getElementById('recorder');
const mic = document.getElementById('dictate');
const timerEl = document.getElementById('timer');
const errorEl = document.getElementById('error');
const liveEl = document.getElementById('live');
const committedEl = document.getElementById('committed');
const pendingEl = document.getElementById('pending');
const saveEl = document.getElementById('save');
const discardEl = document.getElementById('discard');

let speech = null;
let speechPromise = null;
let recorder = null;
let draftId = recorderEl?.dataset.draftId || null;
let committed = committedEl?.textContent?.trim() ?? '';
let textPending = false;
let textSync = null;
let errorTimer = null;

function clearError() {
  clearTimeout(errorTimer);
  if (!errorEl) return;
  errorEl.textContent = '';
  errorEl.hidden = true;
}

// Failures appear in a fixed toast so reporting them never moves the page.
function showError(message) {
  if (!errorEl) return;
  errorEl.textContent = message;
  errorEl.hidden = false;
  clearTimeout(errorTimer);
  errorTimer = setTimeout(clearError, 8000);
}

// Loading is drawn on the record button itself: the ring fills with download
// progress while the button stays visually active instead of going quiet.
function setLoading(progress) {
  if (!mic) return;
  if (progress == null) {
    mic.classList.remove('loading');
    mic.removeAttribute('aria-busy');
    mic.style.removeProperty('--progress');
    return;
  }
  mic.classList.add('loading');
  mic.setAttribute('aria-busy', 'true');
  const percent =
    typeof progress === 'number' && Number.isFinite(progress)
      ? Math.max(0, Math.min(100, Math.round(progress)))
      : 0;
  mic.style.setProperty('--progress', percent + '%');
}

function describe(error) {
  const message = error?.message ? String(error.message) : String(error);
  if (/\b401\b/.test(message)) {
    return 'Your session expired. Reload the page and sign in again.';
  }
  return message || 'Something went wrong.';
}

function appendText(current, addition) {
  const left = current.trim();
  const right = (addition ?? '').trim();
  if (!left) return right;
  if (!right) return left;
  return left + ' ' + right;
}

function updateActions() {
  const hasText = committed.trim().length > 0;
  if (saveEl) saveEl.disabled = !draftId || !hasText || Boolean(recorder);
  if (discardEl) discardEl.disabled = !draftId || Boolean(recorder);
}

function render() {
  if (committedEl) committedEl.textContent = committed;
  if (liveEl) liveEl.hidden = !committed.trim() && !recorder;
  updateActions();
}

async function apiError(response) {
  if (response.status === 401) {
    return new Error('401: not signed in.');
  }
  const data = await response.json().catch(() => null);
  return new Error(data?.error || 'Request failed (' + response.status + ').');
}

async function ensureDraft() {
  if (draftId) return draftId;
  const response = await fetch('/api/drafts', {
    method: 'POST',
    credentials: 'same-origin'
  });
  if (!response.ok) throw await apiError(response);
  const data = await response.json().catch(() => null);
  if (typeof data?.id !== 'string' || !data.id) {
    throw new Error('The server returned an unexpected response.');
  }
  draftId = data.id;
  if (recorderEl) recorderEl.dataset.draftId = draftId;
  window.history.replaceState(null, '', '/?draft=' + encodeURIComponent(draftId));
  updateActions();
  return draftId;
}

async function putDraft(patch) {
  if (!draftId) throw new Error('No draft is open.');
  const response = await fetch('/api/drafts/' + encodeURIComponent(draftId), {
    method: 'PUT',
    credentials: 'same-origin',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(patch)
  });
  if (!response.ok) throw await apiError(response);
}

// Serialize text writes: a chunk arriving during a write marks the draft dirty
// so the latest full text is sent next, rather than racing older text.
function queueTextSync() {
  if (!draftId) return Promise.resolve();
  textPending = true;
  if (textSync) return textSync;
  textSync = (async () => {
    try {
      while (textPending) {
        textPending = false;
        await putDraft({ text: committed });
      }
    } catch (error) {
      textPending = true;
      throw error;
    } finally {
      textSync = null;
    }
  })();
  return textSync;
}

function syncText() {
  queueTextSync().catch(() => {
    showError('The draft could not be saved. It will retry with the next words.');
  });
}

async function flushText() {
  try {
    await queueTextSync();
  } catch {
    // Saving the note carries the current text regardless.
  }
}

function resetSpeech(session) {
  if (speech !== session) return;
  speech = null;
  speechPromise = null;
  session?.close().catch(() => {});
}

async function getSpeech() {
  if (speech) return speech;
  if (!speechPromise) {
    speechPromise = (async () => {
      const { createWhistle } = await import(VENDOR);
      const session = await createWhistle({
        model: MODEL_URL,
        modelSha256: MODEL_SHA256,
        cache: true,
        onDownloadProgress: progress => setLoading(progress?.percentage ?? 0)
      });
      speech = session;
      return session;
    })();
    speechPromise.catch(() => {
      speechPromise = null;
    });
  }
  return speechPromise;
}

function supported() {
  return Boolean(
    navigator.mediaDevices &&
      window.AudioWorkletNode &&
      window.WebAssembly &&
      globalThis.crypto?.subtle
  );
}

function updateTimer() {
  if (!recorder || !timerEl) return;
  const total = Math.floor((Date.now() - recorder.startedAt) / 1000);
  const minutes = String(Math.floor(total / 60)).padStart(2, '0');
  const seconds = String(total % 60).padStart(2, '0');
  timerEl.textContent = minutes + ':' + seconds;
}

function sleep(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}

async function startDictation() {
  if (recorder) return;
  clearError();
  if (!supported()) {
    showError(
      'This browser cannot run the speech model. Use a Chromium-class browser over HTTPS or localhost.'
    );
    return;
  }

  mic.disabled = true;
  setLoading(0);

  try {
    await ensureDraft();
  } catch (error) {
    mic.disabled = false;
    setLoading(null);
    showError(describe(error));
    return;
  }

  let session;
  try {
    session = await getSpeech();
  } catch (error) {
    mic.disabled = false;
    setLoading(null);
    showError(describe(error));
    return;
  }

  let stream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({
      audio: {
        channelCount: 1,
        echoCancellation: true,
        noiseSuppression: true,
        autoGainControl: true
      }
    });
  } catch {
    mic.disabled = false;
    setLoading(null);
    showError('Microphone access was blocked. Allow it and try again.');
    return;
  }

  let ctx;
  let source;
  let node;
  try {
    ctx = new AudioContext();
    await ctx.resume();
    await ctx.audioWorklet.addModule('/static/recorder-worklet.js');
    source = ctx.createMediaStreamSource(stream);
    node = new AudioWorkletNode(ctx, 'pcm-capture');
    const sink = ctx.createGain();
    sink.gain.value = 0;
    source.connect(node).connect(sink).connect(ctx.destination);
  } catch (error) {
    stream.getTracks().forEach(track => track.stop());
    ctx?.close().catch(() => {});
    mic.disabled = false;
    setLoading(null);
    showError('Could not start audio capture: ' + describe(error));
    return;
  }

  const queue = new AsyncQueue();
  const resampler = new StreamingResampler(ctx.sampleRate, 16000);
  const accum = new PcmAccumulator(16000);

  node.port.onmessage = ({ data }) => {
    for (const chunk of accum.push(resampler.process(data))) queue.push(chunk);
  };

  const state = {
    session,
    ctx,
    source,
    node,
    stream,
    queue,
    accum,
    resampler,
    startedAt: Date.now(),
    loopError: null,
    loop: null,
    timer: null,
    stopping: false
  };
  recorder = state;

  if (pendingEl) pendingEl.textContent = '';
  if (liveEl) liveEl.hidden = false;

  state.loop = (async () => {
    for await (const part of state.session.stream(state.queue)) {
      if (part.text) {
        committed = appendText(committed, part.text);
        if (committedEl) committedEl.textContent = committed;
        if (liveEl) liveEl.hidden = false;
        updateActions();
        syncText();
      }
      if (pendingEl) pendingEl.textContent = part.pending ?? '';
    }
  })();
  state.loop.catch(error => {
    state.loopError = error;
  });

  setLoading(null);
  mic.disabled = false;
  mic.classList.add('recording');
  mic.setAttribute('aria-label', 'Pause dictation');
  if (timerEl) {
    timerEl.hidden = false;
    timerEl.textContent = '00:00';
  }
  state.timer = setInterval(updateTimer, 250);
  updateActions();
}

async function stopDictation() {
  const state = recorder;
  if (!state || state.stopping) return;
  state.stopping = true;
  mic.disabled = true;
  clearInterval(state.timer);

  try {
    state.node.port.postMessage('flush');
  } catch {
    // The worklet may already be gone; the accumulator flush below still runs.
  }
  await sleep(100);

  state.node.port.onmessage = null;
  try {
    state.source.disconnect();
  } catch {}
  try {
    state.node.disconnect();
  } catch {}
  state.stream.getTracks().forEach(track => track.stop());

  const rest = state.accum.flush();
  if (rest.length) state.queue.push(rest);
  state.queue.close();

  try {
    await state.loop;
  } catch (error) {
    state.loopError = state.loopError ?? error;
  }
  await state.ctx.close().catch(() => {});

  recorder = null;
  mic.classList.remove('recording');
  mic.setAttribute('aria-label', 'Start note');
  if (timerEl) timerEl.hidden = true;
  if (pendingEl) pendingEl.textContent = '';

  if (state.loopError) {
    const code = state.loopError?.code;
    if (
      code === 'INVALID_ARGUMENT' ||
      code === 'WORKER_ERROR' ||
      code === 'DOWNLOAD_FAILED' ||
      code === 'CLOSED'
    ) {
      resetSpeech(state.session);
    }
  }

  committed = committed.trim();
  render();

  // Do not save here: stopping simply pauses. Persist the draft so a reload or
  // closed tab cannot lose what has already been transcribed.
  await flushText();
  if (state.loopError) showError(describe(state.loopError));
  mic.disabled = false;
  updateActions();
}

async function saveCurrentDraft() {
  if (!draftId || recorder) return;
  clearError();
  if (saveEl) saveEl.disabled = true;
  if (discardEl) discardEl.disabled = true;

  await flushText();

  let response;
  try {
    response = await fetch(
      '/api/drafts/' + encodeURIComponent(draftId) + '/save',
      {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ text: committed })
      }
    );
  } catch {
    showError('Could not reach the server. Your draft is still available.');
    updateActions();
    return;
  }

  if (response.ok) {
    const saved = await response.json().catch(() => null);
    if (typeof saved?.id === 'string' && saved.id) {
      textPending = false;
      window.location.assign('/dictations/' + encodeURIComponent(saved.id));
      return;
    }
    showError('The server returned an unexpected response. Your draft is still available.');
    updateActions();
    return;
  }

  const error = await apiError(response);
  showError(describe(error));
  updateActions();
}

async function discardCurrentDraft() {
  if (!draftId || recorder) return;
  if (!window.confirm('Discard this draft? This cannot be undone.')) return;
  clearError();
  if (saveEl) saveEl.disabled = true;
  if (discardEl) discardEl.disabled = true;

  let response;
  try {
    response = await fetch('/api/drafts/' + encodeURIComponent(draftId), {
      method: 'DELETE',
      credentials: 'same-origin'
    });
  } catch {
    showError('Could not reach the server. The draft was not discarded.');
    updateActions();
    return;
  }

  if (response.ok || response.status === 404) {
    draftId = null;
    textPending = false;
    committed = '';
    window.location.assign('/');
    return;
  }

  const error = await apiError(response);
  showError(describe(error));
  updateActions();
}

if (mic) {
  mic.addEventListener('click', () => {
    if (recorder) stopDictation();
    else startDictation();
  });
}

if (saveEl) {
  saveEl.addEventListener('click', saveCurrentDraft);
}

if (discardEl) {
  discardEl.addEventListener('click', discardCurrentDraft);
}

if (errorEl) {
  errorEl.addEventListener('click', clearError);
}

window.addEventListener('beforeunload', event => {
  if (!draftId || !textPending) return;
  event.preventDefault();
  event.returnValue = '';
});

// Hydrate from the server-rendered draft so pausing, reloading and switching
// drafts all continue from the same text.
committed = committed.trim();
render();

// Localize UTC timestamps when JavaScript is available.
for (const el of document.querySelectorAll('time[data-local]')) {
  const raw = el.getAttribute('datetime');
  const date = new Date(raw ?? '');
  if (!Number.isNaN(date.getTime())) {
    el.textContent = date.toLocaleString(undefined, {
      dateStyle: 'medium',
      timeStyle: 'short'
    });
  }
}
