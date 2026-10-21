// Batches raw microphone input into ~1024-sample chunks for the main thread.
// Resampling happens on the main thread so it stays unit-testable.
class PcmCapture extends AudioWorkletProcessor {
  constructor() {
    super();
    this.buf = new Float32Array(1024);
    this.len = 0;
    this.port.onmessage = () => this.flush();
  }

  flush() {
    if (!this.len) return;
    const out = this.buf.slice(0, this.len);
    this.port.postMessage(out, [out.buffer]);
    this.len = 0;
  }

  process(inputs) {
    const channel = inputs[0]?.[0];
    if (!channel) return true;
    for (let i = 0; i < channel.length; i++) {
      this.buf[this.len++] = channel[i];
      if (this.len === this.buf.length) this.flush();
    }
    return true;
  }
}

registerProcessor('pcm-capture', PcmCapture);
