// The local microphone into the desktop: captured with echo cancellation and noise suppression, encoded
// as Opus by WebCodecs and handed to `sendPacket` one packet at a time; stopping ends the track, so the
// browser's recording indicator goes off.
let stream = null, ctx = null, encoder = null;

export const micOn = () => !!stream;

export async function startMic(sendPacket) {
  if (stream) return;
  stream = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true } });
  ctx = new AudioContext({ sampleRate: 48000 }); // the context resamples the microphone to Opus's rate
  encoder = new AudioEncoder({
    output: chunk => { const b = new ArrayBuffer(chunk.byteLength); chunk.copyTo(b); sendPacket(b); },
    error: e => { console.error('microphone encoder:', e); stopMic(); },
  });
  encoder.configure({ codec: 'opus', sampleRate: ctx.sampleRate, numberOfChannels: 1, bitrate: 48000 });
  // a ScriptProcessor (deprecated, but the one universal way to PCM) hands over 1024-frame blocks, which the
  // encoder cuts into 20 ms packets; its own output stays silent, so nothing is heard locally
  const node = ctx.createScriptProcessor(1024, 1, 1);
  let frames = 0;
  node.onaudioprocess = e => {
    const pcm = e.inputBuffer.getChannelData(0);
    encoder.encode(new AudioData({ format: 'f32-planar', sampleRate: ctx.sampleRate, numberOfFrames: pcm.length, numberOfChannels: 1, timestamp: Math.round((frames * 1e6) / ctx.sampleRate), data: pcm }));
    frames += pcm.length;
  };
  ctx.createMediaStreamSource(stream).connect(node);
  node.connect(ctx.destination);
}

export function stopMic() {
  stream?.getTracks().forEach(t => t.stop());
  ctx?.close();
  if (encoder?.state !== 'closed') encoder?.close();
  stream = ctx = encoder = null;
}
