import workletSource from './mic-worklet.js?raw';

// Capture mono PCM at the context's rate; only the current start may send Opus or report failure.
// Returns true for a live start. onEnd reports capture failures after cleanup; stop ends the tracks
// so the browser's recording indicator turns off.
let active = null;

export async function startMic(sendPacket, onEnd) {
  if (active) return false;
  const capture = {};
  active = capture;
  const current = () => active === capture;
  const fail = error => {
    if (!current()) return;
    stopMic();
    onEnd(error);
  };
  try {
    const ctx = capture.ctx = new AudioContext({ sampleRate: 48000 });
    const enc = capture.encoder = new AudioEncoder({
      output: chunk => {
        if (!current()) return;
        try {
          const b = new ArrayBuffer(chunk.byteLength);
          chunk.copyTo(b);
          sendPacket(b);
        } catch (e) { fail(e); }
      },
      error: fail,
    });
    enc.configure({ codec: 'opus', sampleRate: ctx.sampleRate, numberOfChannels: 1, bitrate: 48000 });
    const stream = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true } });
    if (!current()) { stream.getTracks().forEach(t => t.stop()); return false; }
    capture.stream = stream;
    for (const track of stream.getTracks()) track.onended = () => fail(new Error('Microphone disconnected'));
    let frames = 0;
    const encode = pcm => {
      if (!current()) return;
      let data;
      try {
        data = new AudioData({ format: 'f32-planar', sampleRate: ctx.sampleRate, numberOfFrames: pcm.length, numberOfChannels: 1, timestamp: Math.round(frames * 1e6 / ctx.sampleRate), data: pcm });
        enc.encode(data);
        frames += pcm.length;
      } catch (e) { fail(e); }
      finally { data?.close(); }
    };
    if (ctx.audioWorklet && typeof AudioWorkletNode !== 'undefined') {
      const url = URL.createObjectURL(new Blob([workletSource], { type: 'text/javascript' }));
      try { await ctx.audioWorklet.addModule(url); }
      finally { URL.revokeObjectURL(url); }
      if (!current()) return false;
      const node = capture.node = new AudioWorkletNode(ctx, 'microphone-capture', { channelCount: 1, channelCountMode: 'explicit', outputChannelCount: [1] });
      node.port.onmessage = ({ data }) => encode(data);
      node.onprocessorerror = () => fail(new Error('Microphone processing failed'));
    } else {
      // Compatibility capture for browsers without AudioWorklet.
      const node = capture.node = ctx.createScriptProcessor(1024, 1, 1);
      node.onaudioprocess = e => encode(e.inputBuffer.getChannelData(0));
    }
    const source = capture.source = ctx.createMediaStreamSource(stream);
    source.connect(capture.node);
    capture.node.connect(ctx.destination); // The processor leaves its output silent.
    await ctx.resume();
    return current();
  } catch (e) {
    if (!current()) return false;
    stopMic();
    throw e;
  }
}

export function stopMic() {
  const capture = active;
  active = null;
  if (!capture) return;
  capture.stream?.getTracks().forEach(t => { t.onended = null; t.stop(); });
  capture.source?.disconnect();
  if (capture.node) {
    capture.node.disconnect();
    if (capture.node.port) {
      capture.node.port.onmessage = null;
      capture.node.port.close();
      capture.node.onprocessorerror = null;
    } else capture.node.onaudioprocess = null;
  }
  if (capture.encoder && capture.encoder.state !== 'closed') capture.encoder.close();
  capture.ctx?.close().catch(() => {});
}
