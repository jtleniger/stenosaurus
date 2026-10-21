import { StreamingResampler, PcmAccumulator, AsyncQueue } from '/static/pcm.js';

const VENDOR = '/static/vendor/cactus-needle-0.3.0/dist/browser/index.js';
const MODEL_URL = '/model/whistle.cact';
const MODEL_SHA256 = 'b6e02f048568ac5d01a2042556c658061e699acbc0aa2a1439f52f3d461dffeb';

const mic = document.getElementById('dictate');
const timerEl = document.getElementById('timer');
const statusEl = document.getElementById('status');
const errorEl = document.getElementById('error');
const liveEl = document.getElementById('live');
const committedEl = document.getElementById('committed');
const pendingEl = document.getElementById('pending');

let speech = null;
let speechPromise = null;
let recorder = null;

function setStatus(text) {
  if (statusEl) statusEl.textContent = text;
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
  setStatus('Loading speech model…');

  let session;
  try {
    session = await getSpeech();
  } catch (error) {
    mic.disabled = false;
    setStatus('Start Note');
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
  } catch (error) {
    mic.disabled = false;
    setStatus('Start Note');
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
    setStatus('Start Note');
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
    committed: '',
    loopError: null,
    loop: null,
    timer: null,
    stopping: false
  };
  recorder = state;

  if (committedEl) committedEl.textContent = '';
  if (pendingEl) pendingEl.textContent = '';
  if (liveEl) liveEl.hidden = false;

  state.loop = (async () => {
    for await (const part of session.stream(queue)) {
      if (part.text) state.committed = appendText(state.committed, part.text);
      if (committedEl) committedEl.textContent = state.committed;
      if (pendingEl) pendingEl.textContent = part.pending ?? '';
    }
  })();
  state.loop.catch(error => {
    state.loopError = error;
  });

  mic.disabled = false;
  mic.classList.add('recording');
  mic.setAttribute('aria-label', 'Stop note');
  setStatus('Listening… tap to stop');
  if (timerEl) {
    timerEl.hidden = false;
    timerEl.textContent = '00:00';
  }
  state.timer = setInterval(updateTimer, 250);
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

  if (state.loopError) {
    const code = state.loopError?.code;
    if (code === 'INVALID_ARGUMENT' || code === 'WORKER_ERROR' || code === 'DOWNLOAD_FAILED' || code === 'CLOSED') {
      resetSpeech(state.session);
    }
  }

  const text = state.committed.trim();
  if (text === '') {
    if (liveEl) liveEl.hidden = true;
    setStatus('No speech detected. Start Note.');
    mic.disabled = false;
    if (state.loopError) showError(describe(state.loopError));
    return;
  }

  setStatus('Saving…');
  let response;
  try {
    response = await fetch('/api/dictations', {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ text })
    });
  } catch (error) {
    setStatus('Start Note');
    showError('Could not reach the server. Your text is still shown above.');
    if (state.loopError) showError(describe(state.loopError));
    mic.disabled = false;
    return;
  }

  if (response.ok) {
    const saved = await response.json().catch(() => null);
    if (saved?.id) {
      window.location.assign('/dictations/' + encodeURIComponent(saved.id));
      return;
    }
    setStatus('Start Note');
    showError('The server returned an unexpected response. Your text is still shown above.');
    mic.disabled = false;
    return;
  }

  if (response.status === 401) {
    showError('Your session expired. Reload the page and sign in again.');
  } else {
    const data = await response.json().catch(() => ({}));
    showError(data.error || 'Could not save the note.');
  }
  if (state.loopError) showError(describe(state.loopError));
  setStatus('Start Note');
  if (liveEl) liveEl.hidden = false;
  mic.disabled = false;
}

if (mic) {
  mic.addEventListener('click', () => {
    if (recorder) stopDictation();
    else startDictation();
  });
}

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
