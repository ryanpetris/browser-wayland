// The local webcam into the desktop: 720p at 30 fps when the camera has it, encoded as VP8 by WebCodecs in
// realtime mode at 2 Mbit/s (a keyframe every two seconds, so a dropped frame costs that much at most) and
// handed to `sendFrame` one frame at a time; stopping ends the track, so the browser's camera indicator
// goes off. `onEnd` hears of an encoder failure.
let stream = null, encoder = null, reader = null, generation = 0;

export async function startCam(sendFrame, onEnd) {
  if (stream) return;
  const g = ++generation;
  const s = await navigator.mediaDevices.getUserMedia({ video: { width: 1280, height: 720, frameRate: 30 } });
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
  let frames = 0, skipped = false;
  (async () => {
    for (;;) {
      const { value, done } = await r.read();
      if (done || enc.state !== 'configured') { value?.close(); break; }
      // a frame the encoder hasn't got to yet means the link is behind: this one is skipped, the next is whole
      if (enc.encodeQueueSize > 1) { value.close(); skipped = true; continue; }
      enc.encode(value, { keyFrame: skipped || frames++ % 60 === 0 });
      skipped = false;
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
