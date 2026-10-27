import { StreamingResampler, PcmAccumulator, AsyncQueue } from '/static/pcm.js';

const VENDOR = '/static/vendor/cactus-needle-0.3.0/dist/browser/index.js';
const MODEL_URL = '/model/whistle.cact';
const MODEL_SHA256 = 'b6e02f048568ac5d01a2042556c658061e699acbc0aa2a1439f52f3d461dffeb';

const recorderEl = document.getElementById('recorder');
const mic = document.getElementById('dictate');
const timerEl = document.getElementById('timer');
const statusEl = document.getElementById('status');
const errorEl = document.getElementById('error');
const liveEl = document.getElementById('live');
const committedEl = document.getElementById('committed');
const pendingEl = document.getElementById('pending');
const nameEl = document.getElementById('draft-name');
const saveEl = document.getElementById('save');
const discardEl = document.getElementById('discard');
const draftStatusEl = document.getElementById('draft-status');

let speech = null;
let speechPromise = null;
let recorder = null;
let draftId = recorderEl?.dataset.draftId ? Number(recorderEl.dataset.draftId) : null;
let committed = committedEl?.textContent?.trim() ?? '';
let textPending = false;
let textSync = null;
let nameTimer = null;
let savedName = nameEl?.value ?? '';

function setStatus(text) {
  if (statusEl) statusEl.textContent = text;
}

function setDraftStatus(text, isError = false) {
  if (!draftStatusEl) return;
  draftStatusEl.textContent = text;
  draftStatusEl.classList.toggle('error', isError);
}

function clearError() {
  if (!errorEl) return;
  errorEl.textContent = '';
  errorEl.hidden = true;
}

function showError(message) {
  if (!errorEl) return;
  errorEl.textContent = errorEl.textContent
    ? errorEl.textContent + ' ' + message
    : message;
  errorEl.hidden = false;
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

function idleStatus() {
  if (committed.trim()) return 'Paused. Tap to continue.';
  return draftId ? 'Ready. Tap to dictate.' : 'Start Note';
}

function updateActions() {
  const hasText = committed.trim().length > 0;
  if (saveEl) saveEl.disabled = !draftId || !hasText || Boolean(recorder);
  if (discardEl) discardEl.disabled = !draftId || Boolean(recorder);
  if (nameEl) nameEl.disabled = !draftId;
}

function draftLink() {
  if (!draftId) return null;
  return document.querySelector('#draft-list a[href="/?draft=' + draftId + '"]');
}

function previewText(text) {
  const collapsed = text.split(/\s+/).filter(Boolean).join(' ');
  if (!collapsed) return '(empty)';
  return collapsed.length > 140 ? collapsed.slice(0, 140) + '…' : collapsed;
}

function updateDraftSummary() {
  const link = draftLink();
  if (!link) return;
  const title = link.querySelector('.draft-title');
  if (title) title.textContent = nameEl?.value.trim() || 'Untitled draft';
  const preview = link.querySelector('.preview');
  if (preview) preview.textContent = previewText(committed);
}

function ensureDraftListItem() {
  const list = document.getElementById('draft-list');
  if (!list || !draftId) return;
  const empty = document.getElementById('drafts-empty');
  if (empty) empty.hidden = true;
  list.hidden = false;
  if (draftLink()) {
    updateDraftSummary();
    return;
  }
  const item = document.createElement('li');
  const link = document.createElement('a');
  link.href = '/?draft=' + draftId;
  const title = document.createElement('span');
  title.className = 'draft-title';
  title.textContent = 'Untitled draft';
  const preview = document.createElement('span');
  preview.className = 'preview';
  preview.textContent = previewText(committed);
  const time = document.createElement('time');
  time.textContent = 'Just now';
  link.append(title, preview, time);
  item.append(link);
  list.prepend(item);
}

function render() {
  if (committedEl) committedEl.textContent = committed;
  if (liveEl) liveEl.hidden = !committed.trim() && !recorder;
  updateDraftSummary();
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
  const data = await response.json();
  draftId = Number(data?.id);
  if (!Number.isFinite(draftId)) throw new Error('The server returned an unexpected response.');
  if (recorderEl) recorderEl.dataset.draftId = String(draftId);
  savedName = '';
  const url = new URL(window.location.href);
  url.searchParams.set('draft', String(draftId));
  window.history.replaceState(null, '', url);
  setDraftStatus('');
  ensureDraftListItem();
  updateActions();
  return draftId;
}

async function putDraft(patch) {
  if (!draftId) throw new Error('No draft is open.');
  const response = await fetch('/api/drafts/' + draftId, {
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
        const snapshot = committed;
        await putDraft({ text: snapshot });
        setDraftStatus('Draft saved');
      }
    } catch (error) {
      textPending = true;
      setDraftStatus('Draft not saved — will retry', true);
      throw error;
    } finally {
      textSync = null;
    }
  })();
  return textSync;
}

function syncText() {
  queueTextSync().catch(() => {
    // The save button sends the full text directly, so a failed autosave does
    // not lose the note. Keep the failure visible without flooding the page.
  });
}

async function flushText() {
  try {
    await queueTextSync();
  } catch {
    // Saving the draft carries the current text regardless.
  }
}

function scheduleNameSync() {
  if (!draftId || !nameEl) return;
  clearTimeout(nameTimer);
  nameTimer = setTimeout(syncName, 600);
}

async function syncName() {
  if (!draftId || !nameEl) return;
  const name = nameEl.value;
  if (name === savedName) return;
  try {
    await putDraft({ name });
    savedName = name;
    updateDraftSummary();
    setDraftStatus('Draft saved');
  } catch {
    setDraftStatus('Draft name not saved', true);
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
        onDownloadProgress: progress => {
          const suffix =
            progress?.percentage == null ? '' : ' ' + Math.round(progress.percentage) + '%';
          setStatus('Loading speech model…' + suffix);
        }
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
  setStatus(draftId ? 'Loading speech model…' : 'Creating draft…');

  try {
    await ensureDraft();
  } catch (error) {
    mic.disabled = false;
    setStatus(idleStatus());
    showError(describe(error));
    return;
  }

  setStatus('Loading speech model…');
  let session;
  try {
    session = await getSpeech();
  } catch (error) {
    mic.disabled = false;
    setStatus(idleStatus());
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
    setStatus(idleStatus());
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
    setStatus(idleStatus());
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

  mic.disabled = false;
  mic.classList.add('recording');
  mic.setAttribute('aria-label', 'Pause dictation');
  setStatus('Listening… tap to pause');
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
  setStatus('Finishing…');
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
  setStatus(idleStatus());

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
  setStatus('Saving…');
  if (saveEl) saveEl.disabled = true;
  if (discardEl) discardEl.disabled = true;
  if (nameEl) nameEl.disabled = true;

  await flushText();

  let response;
  try {
    response = await fetch('/api/drafts/' + draftId + '/save', {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ text: committed })
    });
  } catch {
    setStatus(idleStatus());
    showError('Could not reach the server. Your draft is still available.');
    updateActions();
    return;
  }

  if (response.ok) {
    const saved = await response.json().catch(() => null);
    if (saved?.id) {
      textPending = false;
      window.location.assign('/dictations/' + encodeURIComponent(saved.id));
      return;
    }
    setStatus(idleStatus());
    showError('The server returned an unexpected response. Your draft is still available.');
    updateActions();
    return;
  }

  const error = await apiError(response);
  setStatus(idleStatus());
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
    response = await fetch('/api/drafts/' + draftId, {
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

if (nameEl) {
  nameEl.addEventListener('input', () => {
    updateDraftSummary();
    scheduleNameSync();
  });
  nameEl.addEventListener('blur', () => {
    clearTimeout(nameTimer);
    syncName();
  });
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
setStatus(idleStatus());

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
