// The local webcam into the desktop: 720p at 30 fps when the camera has it, encoded as VP8 by WebCodecs in
// realtime mode at 2 Mbit/s (a keyframe every two seconds, so a lost frame costs that much at most) and
// handed to `sendFrame` one frame at a time; stopping ends the track, so the browser's camera indicator
// goes off. `onEnd` hears when it ended on its own: an encoder failure, the camera going away. `behind()`
// says the link is behind (the frame is skipped, the next is a keyframe).
let stream = null, encoder = null, reader = null, generation = 0;

export async function startCam(sendFrame, onEnd, behind) {
  if (stream) return;
  const g = ++generation;
  const s = await navigator.mediaDevices.getUserMedia({ video: { width: 1280, height: 720, frameRate: 30 } });
  if (g !== generation) { s.getTracks().forEach(t => t.stop()); return; } // stopped meanwhile
  stream = s;
  let enc, r, interval;
  try {
    const track = stream.getVideoTracks()[0];
    const { width, height, frameRate = 30 } = track.getSettings();
    interval = Math.round(frameRate * 2);
    enc = encoder = new VideoEncoder({
      output: chunk => { const b = new ArrayBuffer(chunk.byteLength); chunk.copyTo(b); sendFrame(b); },
      error: e => { console.error('webcam encoder:', e); if (g === generation) { stopCam(); onEnd(e); } },
    });
    enc.configure({ codec: 'vp8', width, height, bitrate: 2_000_000, framerate: frameRate, latencyMode: 'realtime' });
    r = reader = new MediaStreamTrackProcessor({ track }).readable.getReader();
  } catch (e) {
    stopCam();
    throw e;
  }
  let frames = 0, skipped = false;
  (async () => {
    try {
      for (;;) {
        const { value, done } = await r.read();
        if (done) break;
        try {
          // a frame the encoder hasn't got to, or a link that is behind: this one is skipped, the next is whole
          if (enc.encodeQueueSize > 1 || behind()) { skipped = true; continue; }
          enc.encode(value, { keyFrame: skipped || frames++ % interval === 0 });
          skipped = false;
        } finally {
          value.close();
        }
      }
    } catch (e) {
      console.error('webcam:', e);
    } finally {
      if (g === generation) { stopCam(); onEnd(); } // ended on its own, not by a stop
    }
  })();
}

export function stopCam() {
  generation++;
  reader?.cancel().catch(() => {});
  stream?.getTracks().forEach(t => t.stop());
  if (encoder?.state !== 'closed') encoder?.close();
  stream = encoder = reader = null;
}
