// Viewer: decodes the H.264 stream with WebCodecs and forwards input.
// Wire format mirrors crates/bw-server/src/protocol.rs.
import { KEYCODES } from './keycodes.js';

const CONFIG = 0x01, VIDEO = 0x02;
const RESIZE = 0x82, MOTION_ABS = 0x83, BUTTON = 0x85, AXIS = 0x86, KEY = 0x87, REQUEST_KEYFRAME = 0x88, BLUR = 0x89;
const BTN = [0x110, 0x112, 0x111, 0x113, 0x114]; // PointerEvent.button -> BTN_LEFT, MIDDLE, RIGHT, SIDE, EXTRA

const canvas = document.getElementById('c');
const ctx = canvas.getContext('2d');
const status = document.getElementById('s');

let ws, decoder, stream, awaitingKey = true, frames = 0;

function connect() {
  ws = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`);
  ws.binaryType = 'arraybuffer';
  ws.onopen = sendResize;
  ws.onmessage = e => onMessage(e.data);
  ws.onclose = e => {
    status.textContent = e.code === 1008 || e.code === 1006 && !stream ? 'unauthorized? open the tokened URL' : 'disconnected, retrying…';
    setTimeout(connect, 1000);
  };
}

function send(type, size, fill) {
  if (ws?.readyState !== WebSocket.OPEN) return;
  const buf = new ArrayBuffer(1 + size), dv = new DataView(buf);
  dv.setUint8(0, type);
  fill?.(dv);
  ws.send(buf);
}

function sendResize() {
  send(RESIZE, 8, dv => {
    dv.setUint16(1, innerWidth, true);
    dv.setUint16(3, innerHeight, true);
    dv.setFloat32(5, devicePixelRatio, true);
  });
}

async function onMessage(buf) {
  const dv = new DataView(buf);
  switch (dv.getUint8(0)) {
    case CONFIG: {
      stream = JSON.parse(new TextDecoder().decode(new Uint8Array(buf, 1)));
      canvas.width = stream.width;
      canvas.height = stream.height;
      decoder?.close();
      decoder = new VideoDecoder({
        output: frame => { ctx.drawImage(frame, 0, 0); frame.close(); frames++; },
        error: e => { console.error(e); awaitingKey = true; send(REQUEST_KEYFRAME, 0); },
      });
      const cfg = { codec: stream.codec, optimizeForLatency: true, hardwareAcceleration: 'prefer-hardware' };
      const hw = await VideoDecoder.isConfigSupported(cfg);
      decoder.configure(hw.supported ? cfg : { ...cfg, hardwareAcceleration: 'no-preference' });
      awaitingKey = true;
      status.textContent = `${stream.codec} ${stream.width}×${stream.height} ${hw.supported ? 'hw' : 'sw'}`;
      break;
    }
    case VIDEO: {
      if (!decoder || decoder.state !== 'configured') return;
      const key = (dv.getUint8(1) & 1) !== 0;
      if (!key && (awaitingKey || decoder.decodeQueueSize > 4)) {
        if (!awaitingKey) { awaitingKey = true; send(REQUEST_KEYFRAME, 0); }
        return;
      }
      awaitingKey = false;
      decoder.decode(new EncodedVideoChunk({
        type: key ? 'key' : 'delta',
        timestamp: Number(dv.getBigUint64(2, true)),
        data: new Uint8Array(buf, 10),
      }));
    }
  }
}

// --- input -----------------------------------------------------------------

// Stream logical px per canvas CSS px (1 except while a resize is in flight).
const scaleX = () => stream ? stream.width / stream.scale / canvas.clientWidth : 1;
const scaleY = () => stream ? stream.height / stream.scale / canvas.clientHeight : 1;

canvas.onpointermove = e => send(MOTION_ABS, 8, dv => {
  dv.setFloat32(1, e.offsetX * scaleX(), true);
  dv.setFloat32(5, e.offsetY * scaleY(), true);
});
canvas.onpointerdown = canvas.onpointerup = e => {
  if (e.type === 'pointerdown') canvas.setPointerCapture(e.pointerId);
  canvas.onpointermove(e);
  send(BUTTON, 3, dv => { dv.setUint16(1, BTN[e.button] ?? 0x110, true); dv.setUint8(3, e.type === 'pointerdown' ? 1 : 0); });
};
canvas.oncontextmenu = e => e.preventDefault();
canvas.addEventListener('wheel', e => {
  e.preventDefault();
  send(AXIS, 9, dv => { dv.setUint8(1, e.deltaMode); dv.setFloat32(2, e.deltaX, true); dv.setFloat32(6, e.deltaY, true); });
}, { passive: false });

const onKey = e => {
  const code = KEYCODES[e.code];
  if (!code || e.repeat) return; // clients repeat keys themselves (wl_keyboard.repeat_info)
  e.preventDefault();
  send(KEY, 3, dv => { dv.setUint16(1, code, true); dv.setUint8(3, e.type === 'keydown' ? 1 : 0); });
};
window.onkeydown = window.onkeyup = onKey;
window.onblur = () => send(BLUR, 0);
document.onvisibilitychange = () => { if (document.hidden) send(BLUR, 0); };

let resizeTimer;
window.onresize = () => { clearTimeout(resizeTimer); resizeTimer = setTimeout(sendResize, 150); };

connect();
