// The local webcam into the desktop: 720p at 30 fps when the camera has it, encoded as VP8 by WebCodecs in
// realtime mode at 2 Mbit/s (a keyframe every two seconds, so a dropped frame costs that much at most) and
// handed to `sendFrame` one frame at a time; stopping ends the track, so the browser's camera indicator
// goes off. `onEnd` hears of an encoder failure. `constraints` may pick a device.
let stream = null, encoder = null, reader = null, generation = 0;

export async function startCam(sendFrame, onEnd, constraints = {}) {
  if (stream) return;
  const g = ++generation;
  let s;
  try {
    s = await navigator.mediaDevices.getUserMedia({ video: { width: 1280, height: 720, frameRate: 30, ...constraints } });
  } catch (e) {
    throw e;
  }
  if (g !== generation) { s.getTracks().forEach(t => t.stop()); return; } // stopped meanwhile
  stream = s;
  const track = stream.getVideoTracks()[0];
  const { width, height, frameRate } = track.getSettings();
  const enc = (encoder = new VideoEncoder({
    output: chunk => { const b = new ArrayBuffer(chunk.byteLength); chunk.copyTo(b); sendFrame(b); },
    error: e => { console.error('webcam encoder:', e); stopCam(); onEnd(e); },
  }));
  enc.configure({ codec: 'vp8', width, height, bitrate: 2_000_000, framerate: frameRate ?? 30, latencyMode: 'realtime' });
  reader = new MediaStreamTrackProcessor({ track }).readable.getReader();
  const r = reader;
  let frames = 0;
  (async () => {
    for (;;) {
      const { value, done } = await r.read();
      if (done || enc.state !== 'configured') { value?.close(); break; }
      enc.encode(value, { keyFrame: frames++ % 60 === 0 });
      value.close();
    }
  })().catch(() => {});
}

export function stopCam() {
  generation++;
  reader?.cancel().catch(() => {});
  stream?.getTracks().forEach(t => t.stop());
  if (encoder?.state !== 'closed') encoder?.close();
  stream = encoder = reader = null;
}
