// Batch mono input into 1024-frame PCM blocks. Untouched output buffers are silent.
class MicrophoneCapture extends AudioWorkletProcessor {
  constructor() {
    super();
    this.pcm = new Float32Array(1024);
    this.used = 0;
  }

  process(inputs) {
    const input = inputs[0]?.[0];
    if (input) {
      for (let offset = 0; offset < input.length;) {
        const count = Math.min(input.length - offset, this.pcm.length - this.used);
        this.pcm.set(input.subarray(offset, offset + count), this.used);
        this.used += count;
        offset += count;
        if (this.used === this.pcm.length) {
          // Structured cloning snapshots the samples before this buffer is reused.
          this.port.postMessage(this.pcm);
          this.used = 0;
        }
      }
    }
    return true;
  }
}

registerProcessor('microphone-capture', MicrophoneCapture);
