import test from 'node:test';
import assert from 'node:assert/strict';

import { StreamingResampler, PcmAccumulator, AsyncQueue } from '../static/pcm.js';

function sine({ rate, seconds = 1, hz = 440, amplitude = 0.5 }) {
  const samples = new Float32Array(Math.round(rate * seconds));
  for (let i = 0; i < samples.length; i++) {
    samples[i] = amplitude * Math.sin((2 * Math.PI * hz * i) / rate);
  }
  return samples;
}

function resampleAll(resampler, input, blockSize) {
  const parts = [];
  for (let offset = 0; offset < input.length; offset += blockSize) {
    parts.push(resampler.process(input.subarray(offset, offset + blockSize)));
  }
  const total = parts.reduce((sum, part) => sum + part.length, 0);
  const out = new Float32Array(total);
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

function peak(samples) {
  let max = 0;
  for (const sample of samples) max = Math.max(max, Math.abs(sample));
  return max;
}

test('16 kHz input passes through unchanged', () => {
  const input = new Float32Array([0.1, -0.2, 0.3, 0]);
  const resampler = new StreamingResampler(16000, 16000);
  assert.deepEqual(Array.from(resampler.process(input)), Array.from(input));
});

test('48 kHz 440 Hz resamples to 16000 samples per second', () => {
  const input = sine({ rate: 48000 });
  const out = resampleAll(new StreamingResampler(48000, 16000), input, 1024);
  assert.ok(Math.abs(out.length - 16000) <= 5, `got ${out.length} samples`);
  for (const sample of out) assert.ok(Number.isFinite(sample));
  const max = peak(out);
  assert.ok(max > 0.48 && max < 0.52, `peak ${max}`);
});

test('44.1 kHz 440 Hz resamples to about 16000 samples per second', () => {
  const input = sine({ rate: 44100 });
  const out = resampleAll(new StreamingResampler(44100, 16000), input, 1024);
  assert.ok(Math.abs(out.length - 16000) <= 5, `got ${out.length} samples`);
  assert.ok(peak(out) > 0.47 && peak(out) < 0.53, `peak ${peak(out)}`);
});

test('accumulator emits full chunks and flushes the remainder', () => {
  const accumulator = new PcmAccumulator(16000);
  const input = new Float32Array(40000);
  for (let i = 0; i < input.length; i++) input[i] = i;

  const chunks = accumulator.push(input);
  assert.equal(chunks.length, 2);
  assert.equal(chunks[0].length, 16000);
  assert.equal(chunks[1].length, 16000);
  assert.equal(chunks[0][0], 0);
  assert.equal(chunks[1][0], 16000);

  const rest = accumulator.flush();
  assert.equal(rest.length, 8000);
  assert.equal(rest[0], 32000);
  assert.equal(accumulator.flush().length, 0);
});

test('accumulator joins split pushes across boundaries', () => {
  const accumulator = new PcmAccumulator(4);
  assert.equal(accumulator.push(new Float32Array([1, 2])).length, 0);
  const chunks = accumulator.push(new Float32Array([3, 4, 5, 6]));
  assert.equal(chunks.length, 1);
  assert.deepEqual(Array.from(chunks[0]), [1, 2, 3, 4]);
  assert.deepEqual(Array.from(accumulator.flush()), [5, 6]);
});

test('async queue preserves order and resolves waiters', async () => {
  const queue = new AsyncQueue();
  const iterator = queue[Symbol.asyncIterator]();

  queue.push('a');
  queue.push('b');
  assert.deepEqual(await iterator.next(), { value: 'a', done: false });
  assert.deepEqual(await iterator.next(), { value: 'b', done: false });

  const waiting = iterator.next();
  queue.push('c');
  assert.deepEqual(await waiting, { value: 'c', done: false });

  const closed = iterator.next();
  queue.close();
  assert.deepEqual(await closed, { done: true });
  assert.deepEqual(await iterator.next(), { done: true });
});

test('async queue ignores pushes after close', async () => {
  const queue = new AsyncQueue();
  queue.close();
  queue.push('ignored');
  const iterator = queue[Symbol.asyncIterator]();
  assert.deepEqual(await iterator.next(), { done: true });
});
