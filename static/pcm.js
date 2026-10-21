// Pure-PCM helpers for the browser dictation pipeline.
// No DOM or Web Audio globals: this module is unit-tested under Node.

/**
 * Streaming linear resampler with a one-pole anti-alias low-pass.
 * Preserves state between calls so chunk boundaries never click.
 */
export class StreamingResampler {
  constructor(inputRate, outputRate = 16000) {
    this.step = inputRate / outputRate;
    this.pos = 0;
    this.last = 0;
    this.enabled = inputRate !== outputRate;
    const fc = 7000;
    this.alpha = 1 - Math.exp(-2 * Math.PI * fc / inputRate);
    this.lp = 0;
  }

  process(input) {
    if (!input.length) return input;
    if (!this.enabled) return input;
    const filtered = new Float32Array(input.length);
    for (let i = 0; i < input.length; i++) {
      this.lp += this.alpha * (input[i] - this.lp);
      filtered[i] = this.lp;
    }
    const out = [];
    let pos = this.pos;
    while (pos < filtered.length) {
      const i = Math.floor(pos);
      const frac = pos - i;
      const a = i === 0 ? this.last : filtered[i - 1];
      const b = filtered[i];
      out.push(a + (b - a) * frac);
      pos += this.step;
    }
    this.pos = pos - filtered.length;
    this.last = filtered[filtered.length - 1];
    return new Float32Array(out);
  }
}

/** Collects arbitrary-sized Float32 chunks into fixed-size chunks. */
export class PcmAccumulator {
  constructor(chunkSize = 16000) {
    this.chunkSize = chunkSize;
    this.buf = new Float32Array(0);
  }

  push(input) {
    const merged = new Float32Array(this.buf.length + input.length);
    merged.set(this.buf);
    merged.set(input, this.buf.length);
    const out = [];
    let offset = 0;
    while (merged.length - offset >= this.chunkSize) {
      out.push(merged.slice(offset, offset + this.chunkSize));
      offset += this.chunkSize;
    }
    this.buf = merged.slice(offset);
    return out;
  }

  flush() {
    const out = this.buf;
    this.buf = new Float32Array(0);
    return out;
  }
}

/** Unbounded async queue used as the stream input for Whistle. */
export class AsyncQueue {
  constructor() {
    this.items = [];
    this.waiters = [];
    this.closed = false;
  }

  push(item) {
    if (this.closed) return;
    const waiter = this.waiters.shift();
    if (waiter) waiter({ value: item, done: false });
    else this.items.push(item);
  }

  close() {
    this.closed = true;
    for (const waiter of this.waiters.splice(0)) waiter({ done: true });
  }

  [Symbol.asyncIterator]() {
    return {
      next: () => {
        if (this.items.length) return Promise.resolve({ value: this.items.shift(), done: false });
        if (this.closed) return Promise.resolve({ done: true });
        return new Promise(resolve => this.waiters.push(resolve));
      },
      return: () => {
        this.close();
        return Promise.resolve({ done: true });
      }
    };
  }
}
