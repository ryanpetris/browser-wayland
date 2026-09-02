// Viewer: decodes the H.264 stream with WebCodecs and forwards input.
// Wire format mirrors crates/bw-server/src/protocol.rs.
import { KEYCODES } from './keycodes.js';

const CONFIG = 0x01, VIDEO = 0x02, CURSOR = 0x03, POINTER_LOCK = 0x04;
const RESIZE = 0x82, MOTION_ABS = 0x83, MOTION_REL = 0x84, BUTTON = 0x85, AXIS = 0x86, KEY = 0x87, REQUEST_KEYFRAME = 0x88, BLUR = 0x89;
const BTN = [0x110, 0x112, 0x111, 0x113, 0x114]; // PointerEvent.button -> BTN_LEFT, MIDDLE, RIGHT, SIDE, EXTRA

const canvas = document.getElementById('c');
const ctx = canvas.getContext('2d');
const status = document.getElementById('s');

let ws, decoder, stream, awaitingKey = true, frames = 0, lockRequests = 0, lockError = '';

function connect() {
  ws = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`);
  ws.binaryType = 'arraybuffer';
  ws.onopen = sendResize;
  ws.onmessage = e => onMessage(e.data);
  ws.onclose = () => {
    status.textContent = stream ? 'disconnected, retrying…' : 'no stream; open the URL with ?token= printed by the server';
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

// A decode error closes the decoder for good, so recovery means a fresh one plus a keyframe.
function newDecoder() {
  decoder?.close();
  const d = new VideoDecoder({
    output: frame => { try { ctx.drawImage(frame, 0, 0); frames++; } finally { frame.close(); } },
    error: e => { console.error(e); if (d === decoder) resync(); },
  });
  d.configure({ codec: stream.codec, optimizeForLatency: true });
  decoder = d;
}

function resync() {
  newDecoder();
  awaitingKey = true;
  send(REQUEST_KEYFRAME, 0);
}

function onMessage(buf) {
  const dv = new DataView(buf);
  switch (dv.getUint8(0)) {
    case CONFIG:
      stream = JSON.parse(new TextDecoder().decode(new Uint8Array(buf, 1)));
      canvas.width = stream.width;
      canvas.height = stream.height;
      resync();
      status.textContent = `${stream.codec} ${stream.width}×${stream.height}`;
      break;
    case CURSOR: {
      // The compositor doesn't draw the pointer; we do, with zero latency.
      const w = dv.getUint16(1, true), h = dv.getUint16(3, true);
      if (!w || !h) { canvas.style.cursor = 'none'; break; }
      const c = document.createElement('canvas');
      c.width = w; c.height = h;
      c.getContext('2d').putImageData(new ImageData(new Uint8ClampedArray(buf, 9, w * h * 4), w, h), 0, 0);
      canvas.style.cursor = `url(${c.toDataURL()}) ${dv.getInt16(5, true)} ${dv.getInt16(7, true)}, default`;
      break;
    }
    case POINTER_LOCK:
      // A client locked the pointer (a game, say): lock the browser's too and send raw deltas.
      if (dv.getUint8(1)) {
        lockRequests++;
        canvas.requestPointerLock({ unadjustedMovement: true })?.catch?.(e => {
          lockError = String(e);
          canvas.requestPointerLock()?.catch?.(e2 => { lockError += ' / ' + e2; });
        });
      }
      else if (document.pointerLockElement) document.exitPointerLock();
      break;
    case VIDEO: {
      if (!decoder) return;
      const key = (dv.getUint8(1) & 1) !== 0;
      if (!key && (awaitingKey || decoder.decodeQueueSize > 4)) {
        if (!awaitingKey) { awaitingKey = true; send(REQUEST_KEYFRAME, 0); }
        return;
      }
      try {
        decoder.decode(new EncodedVideoChunk({
          type: key ? 'key' : 'delta',
          timestamp: Number(dv.getBigUint64(2, true)),
          data: new Uint8Array(buf, 10),
          transfer: [buf],
        }));
        awaitingKey = false;
      } catch (e) {
        console.error(e);
        resync();
      }
    }
  }
}

// --- input -----------------------------------------------------------------

// Stream logical px per canvas CSS px (1 except while a resize is in flight).
const scaleX = () => stream ? stream.width / stream.scale / canvas.clientWidth : 1;
const scaleY = () => stream ? stream.height / stream.scale / canvas.clientHeight : 1;

canvas.onpointermove = e => document.pointerLockElement
  ? send(MOTION_REL, 8, dv => { dv.setFloat32(1, e.movementX, true); dv.setFloat32(5, e.movementY, true); })
  : send(MOTION_ABS, 8, dv => { dv.setFloat32(1, e.offsetX * scaleX(), true); dv.setFloat32(5, e.offsetY * scaleY(), true); });
canvas.onpointerdown = canvas.onpointerup = e => {
  const btn = BTN[e.button];
  if (btn === undefined) return;
  if (e.type === 'pointerdown') canvas.setPointerCapture(e.pointerId);
  canvas.onpointermove(e);
  send(BUTTON, 3, dv => { dv.setUint16(1, btn, true); dv.setUint8(3, e.type === 'pointerdown' ? 1 : 0); });
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

// Fullscreen + Keyboard Lock: Ctrl+W, Ctrl+T, Alt+Tab… reach the Wayland clients instead of the browser.
document.getElementById('fs').onclick = async () => {
  await document.documentElement.requestFullscreen();
  navigator.keyboard?.lock?.();
};
document.onfullscreenchange = () => { if (!document.fullscreenElement) navigator.keyboard?.unlock?.(); };

window.bw = () => ({ frames, stream, awaitingKey, lockRequests, lockError, locked: !!document.pointerLockElement, decoder: decoder?.state, queue: decoder?.decodeQueueSize });
connect();
