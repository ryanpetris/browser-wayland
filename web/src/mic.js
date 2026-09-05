// The local microphone into the desktop: captured with echo cancellation and noise suppression, encoded
// as Opus by WebCodecs and handed to `sendPacket` one packet at a time; stopping ends the track, so the
// browser's recording indicator goes off. `onEnd` hears of an encoder failure (the microphone is off then).
let stream = null, ctx = null, encoder = null, generation = 0;

export async function startMic(sendPacket, onEnd) {
  if (stream || ctx) return;
  const g = ++generation;
  // the context and the encoder first: nothing to let go of if they fail, and the gesture is still fresh
  ctx = new AudioContext({ sampleRate: 48000 }); // the context resamples the microphone to Opus's rate
  const enc = (encoder = new AudioEncoder({
    output: chunk => { const b = new ArrayBuffer(chunk.byteLength); chunk.copyTo(b); sendPacket(b); },
    error: e => { console.error('microphone encoder:', e); stopMic(); onEnd(e); },
  }));
  enc.configure({ codec: 'opus', sampleRate: ctx.sampleRate, numberOfChannels: 1, bitrate: 48000 });
  let s;
  try {
    s = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true } });
  } catch (e) {
    if (g === generation) stopMic();
    throw e;
  }
  if (g !== generation) { s.getTracks().forEach(t => t.stop()); return; } // stopped (or started again) meanwhile
  stream = s;
  // a ScriptProcessor (deprecated, but the one universal way to PCM) hands over 1024-frame blocks, which the
  // encoder cuts into 20 ms packets; its own output stays silent, so nothing is heard locally
  const node = ctx.createScriptProcessor(1024, 1, 1), rate = ctx.sampleRate;
  let frames = 0;
  node.onaudioprocess = e => {
    if (enc.state !== 'configured') return; // a block still in flight after stop
    const pcm = e.inputBuffer.getChannelData(0);
    const data = new AudioData({ format: 'f32-planar', sampleRate: rate, numberOfFrames: pcm.length, numberOfChannels: 1, timestamp: Math.round((frames * 1e6) / rate), data: pcm });
    enc.encode(data);
    data.close();
    frames += pcm.length;
  };
  ctx.createMediaStreamSource(stream).connect(node);
  node.connect(ctx.destination);
  ctx.resume(); // made before the capture, the context may have been kept suspended; a capturing page may run one
}

export function stopMic() {
  generation++; // a start still waiting for permission lets its stream go
  stream?.getTracks().forEach(t => t.stop());
  ctx?.close();
  if (encoder?.state !== 'closed') encoder?.close();
  stream = ctx = encoder = null;
}
