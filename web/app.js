// Viewer: decodes the H.264 stream with WebCodecs and forwards input.
// Wire format mirrors crates/bw-server/src/protocol.rs.
import { KEYCODES } from './keycodes.js';
import { TOKEN, api, initDesktop, onWindows, renderBorders, fetchElements, control, getWindows, snapshot, elementsOf } from './desktop.js';

const CONFIG = 0x01, VIDEO = 0x02, CURSOR = 0x03, POINTER_LOCK = 0x04, AUDIO = 0x05, WINDOWS = 0x06, CLIPBOARD = 0x07;
const AUTH = 0x80, HELLO = 0x81, RESIZE = 0x82, MOTION_ABS = 0x83, MOTION_REL = 0x84, BUTTON = 0x85, AXIS = 0x86, KEY = 0x87, REQUEST_KEYFRAME = 0x88, BLUR = 0x89, POINTER_LOCK_LOST = 0x8A, CONTROL = 0x8B, SET_CLIPBOARD = 0x8C;
const BTN = [0x110, 0x112, 0x111, 0x113, 0x114]; // PointerEvent.button -> BTN_LEFT, MIDDLE, RIGHT, SIDE, EXTRA

const canvas = document.getElementById('c');
const status = document.getElementById('s');
// The token (see desktop.js) is the first message on the socket and the bearer header on API calls. No cookies.
let draw, renderer; // set by initRenderer(): paints one VideoFrame
let pendingFrame = null, rafId = 0;

// Paint from requestAnimationFrame only: drawing a WebGPU canvas outside the animation-frame cycle
// makes Chromium present a cleared texture now and then (visible as black flicker). One frame per
// display refresh; if two arrive in between, the older one is dropped.
function schedule(frame) {
  if (pendingFrame) { inflight.delete(pendingFrame.timestamp); pendingFrame.close(); }
  pendingFrame = frame;
  if (!rafId) rafId = requestAnimationFrame(paint);
}
function paint() {
  rafId = 0;
  const frame = pendingFrame;
  pendingFrame = null;
  if (frame) paintNow(frame);
}
// The 2D canvas is fine with immediate draws, and it saves up to one display refresh of latency.
function paintNow(frame) {
  const pts = frame.timestamp; // before draw() closes the frame
  try { draw(frame); frames++; windowFrames++; } catch (e) { console.error(e); frame.close(); }
  if (lastInput) { latencyMs = performance.now() - lastInput; lastInput = 0; } // input -> next painted frame
  if (stats.hidden) { inflight.clear(); return; }
  const t = performance.now(), rec = inflight.get(pts);
  inflight.delete(pts);
  if (rec?.output) { stage.decode.push(rec.output - rec.at); stage.paint.push(t - rec.output); }
  if (lastPaint) stage.interval.push(t - lastPaint);
  lastPaint = t;
}

// --- stats overlay -----------------------------------------------------------
// Per-stage timings of the last second (ms): receive→decoder output, output→paint, paint→paint.
// Frames in flight are keyed by pts (several can be queued in the decoder or waiting for rAF).
const stage = { decode: [], paint: [], interval: [] };
const inflight = new Map(); // pts -> {at, output}
let lastPaint = 0, sinceKey = 0, audioUnderruns = 0;
const stats = document.getElementById('stats');
const pct = (a, p) => a.length ? a.slice().sort((x, y) => x - y)[Math.max(0, Math.ceil(a.length * p) - 1)].toFixed(1) : '-';
function renderStats() {
  if (stats.hidden) return;
  const s = window.bw(), au = s.audio;
  stats.textContent = [
    stream ? `${stream.codec} ${stream.width}×${stream.height} @${stream.scale.toFixed(2)} ${renderer ?? '-'}` : 'no stream yet',
    `fps ${fps}  ${mbps.toFixed(1)} Mbit/s  input→paint ${latencyMs.toFixed(0)} ms`,
    `recv→decoded p50/p95 ${pct(stage.decode, .5)}/${pct(stage.decode, .95)} ms   decoded→paint ${pct(stage.paint, .5)}/${pct(stage.paint, .95)} ms`,
    `paint interval p50/p95 ${pct(stage.interval, .5)}/${pct(stage.interval, .95)} ms   decode queue ${s.queue ?? '-'}`,
    `frames ${frames}  received ${received}  keyframes ${keyframes} (${sinceKey} since last)  lost ${lost}  dropped ${dropped}  errors ${decodeErrors}`,
    `audio ${au ? `${au.state} packets ${au.packets} decoded ${au.decoded} lead ${((nextPlay - (audioCtx?.currentTime ?? 0)) * 1000).toFixed(0)} ms underruns ${audioUnderruns}` : 'off'}`,
    `ws connects ${connects}  closes ${closes.length}  pointer lock ${s.locked} (${lockRequests} requests${lockError ? ', ' + lockError : ''})`,
  ].join('\n');
  stage.decode.length = stage.paint.length = stage.interval.length = 0;
}
document.getElementById('stats-btn').onclick = () => {
  stats.hidden = !stats.hidden;
  try { localStorage.setItem('bw.stats', stats.hidden ? '0' : '1'); } catch {}
  inflight.clear(); stage.decode.length = stage.paint.length = stage.interval.length = 0; lastPaint = 0;
  renderStats();
};
try { stats.hidden = localStorage.getItem('bw.stats') !== '1'; } catch {}

// --- rendering -------------------------------------------------------------

// WebGPU imports the decoded frame as an external texture (zero-copy) and draws it with a
// full-screen triangle; the browser does the YUV->RGB conversion inside the sampler.
async function initWebGPU() {
  const adapter = await navigator.gpu?.requestAdapter();
  if (!adapter) return null;
  const device = await adapter.requestDevice();
  device.lost.then(info => { console.warn('WebGPU device lost:', info.reason, info.message); location.reload(); }); // ponytail: a reload is the simplest recovery
  const context = canvas.getContext('webgpu');
  const format = navigator.gpu.getPreferredCanvasFormat();
  context.configure({ device, format, alphaMode: 'opaque' });
  const module = device.createShaderModule({ code: `
    struct V { @builtin(position) pos: vec4f, @location(0) uv: vec2f }
    @vertex fn vs(@builtin(vertex_index) i: u32) -> V {
      let p = array(vec2f(-1, -1), vec2f(3, -1), vec2f(-1, 3));
      var o: V;
      o.pos = vec4f(p[i], 0, 1);
      o.uv = vec2f(p[i].x * 0.5 + 0.5, 0.5 - p[i].y * 0.5);
      return o;
    }
    @group(0) @binding(0) var s: sampler;
    @group(0) @binding(1) var t: texture_external;
    @fragment fn fs(v: V) -> @location(0) vec4f { return textureSampleBaseClampToEdge(t, s, v.uv); }` });
  const pipeline = device.createRenderPipeline({ layout: 'auto', vertex: { module }, fragment: { module, targets: [{ format }] } });
  const sampler = device.createSampler({ magFilter: 'linear', minFilter: 'linear' });
  return frame => {
    const bindGroup = device.createBindGroup({
      layout: pipeline.getBindGroupLayout(0),
      entries: [{ binding: 0, resource: sampler }, { binding: 1, resource: device.importExternalTexture({ source: frame }) }],
    });
    const encoder = device.createCommandEncoder();
    const pass = encoder.beginRenderPass({
      colorAttachments: [{ view: context.getCurrentTexture().createView(), loadOp: 'clear', storeOp: 'store' }],
    });
    pass.setPipeline(pipeline);
    pass.setBindGroup(0, bindGroup);
    pass.draw(3);
    pass.end();
    device.queue.submit([encoder.finish()]);
    // Only release the frame to the decoder once the GPU has actually sampled it.
    device.queue.onSubmittedWorkDone().then(() => frame.close(), () => frame.close());
  };
}

// `draw` takes ownership of the frame and closes it when done.
// WebGPU is opt-in (`?renderer=webgpu`): Chromium on Linux presents a blank frame now and then when the
// canvas samples decoder frames as external textures (measured ~2% of presented frames), which shows as flicker.
async function initRenderer() {
  if (new URLSearchParams(location.search).get('renderer') === 'webgpu') {
    try { draw = await initWebGPU(); } catch (e) { console.warn('WebGPU unavailable:', e); }
  }
  renderer = draw ? 'webgpu' : '2d';
  if (!draw) {
    const ctx = canvas.getContext('2d');
    draw = frame => { try { ctx.drawImage(frame, 0, 0); } finally { frame.close(); } };
  }
}

let ws, decoder, stream, awaitingKey = true, frames = 0, received = 0, fps = 0, mbps = 0, windowFrames = 0, windowBytes = 0, lastInput = 0, latencyMs = 0, lockRequests = 0, lockError = '', wantLock = false, connects = 0, closes = [], keyframes = 0, decodeErrors = 0, dropped = 0;
let videoSeq = -1, audioSeq = -1, lost = 0, dropNext = false; // seq: last message seen per stream; lost: gaps in either

// No usable token: a paste box instead of a connection attempt.
function askToken(why) {
  try { sessionStorage.removeItem('bw.token'); } catch {}
  status.textContent = why;
  const form = document.getElementById('tokenform');
  form.hidden = false;
  form.onsubmit = e => {
    e.preventDefault();
    const t = form.token.value.trim();
    if (!t) return;
    try { sessionStorage.setItem('bw.token', t); } catch {}
    location.reload();
  };
  form.token.focus();
}

function connect() {
  if (!TOKEN) { askToken('no token: paste the one the server printed'); return; }
  audioSeq = -1; // the server kept counting while we were away
  connects++;
  ws = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`);
  ws.binaryType = 'arraybuffer';
  ws.onopen = async () => {
    const t = new TextEncoder().encode(TOKEN);
    send(AUTH, t.length, dv => new Uint8Array(dv.buffer, 1).set(t));
    await sendHello();
    sendResize();
  };
  ws.onmessage = e => onMessage(e.data);
  ws.onclose = e => {
    closes.push(`${e.code}:${e.reason}`);
    if (e.code === 4001) { stream = null; askToken(`${e.reason || 'wrong token'}: paste the token the server printed`); return; }
    if (e.code === 4002) { status.textContent = 'replaced by another viewer (only one at a time); reload to take over'; return; }
    status.textContent = stream ? 'disconnected, retrying…' : 'no stream, retrying…';
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

// A window action or spawn for the compositor, as JSON.
function sendControl(obj) {
  const body = new TextEncoder().encode(JSON.stringify(obj));
  send(CONTROL, body.length, dv => new Uint8Array(dv.buffer, 1).set(body));
}

// Which codec families this browser decodes, in hardware and at all (bit0 H.264, bit1 HEVC, bit2 VP9).
async function sendHello() {
  const probes = ['avc1.640028', 'hev1.1.6.L120.90', 'vp09.00.40.08'];
  let hw = 0, sw = 0;
  for (const [i, codec] of probes.entries()) {
    const ok = async hardwareAcceleration => (await VideoDecoder.isConfigSupported({ codec, hardwareAcceleration }).catch(() => ({}))).supported;
    if (await ok('prefer-hardware')) hw |= 1 << i;
    if (await ok('no-preference')) sw |= 1 << i;
  }
  send(HELLO, 2, dv => { dv.setUint8(1, hw); dv.setUint8(2, sw); });
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
  if (decoder && decoder.state !== 'closed') decoder.close(); // a decode error closes it already
  const d = new VideoDecoder({
    output: f => { const rec = inflight.get(f.timestamp); if (rec) rec.output = performance.now(); (renderer === 'webgpu' ? schedule : paintNow)(f); },
    error: e => { console.error(e); decodeErrors++; if (d === decoder) resync(); },
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
  windowBytes += buf.byteLength;
  const dv = new DataView(buf);
  switch (dv.getUint8(0)) {
    case CONFIG:
      stream = JSON.parse(new TextDecoder().decode(new Uint8Array(buf, 1)));
      videoSeq = -1; // a new stream counts from 0
      if (pendingFrame) { pendingFrame.close(); pendingFrame = null; } // don't paint the old stream into the new canvas
      canvas.width = stream.width;
      canvas.height = stream.height;
      resync();
      updateStatus();
      fetchElements(); // the scale may have changed
      renderBorders(); // the window list usually arrives before the first Config
      break;
    case CURSOR: {
      // The compositor doesn't draw the pointer; we do, with zero latency.
      const w = dv.getUint16(1, true), h = dv.getUint16(3, true);
      if (!w || !h) { canvas.style.cursor = 'none'; break; }
      const hx = dv.getInt16(5, true), hy = dv.getInt16(7, true), lw = dv.getUint16(9, true) || w, lh = dv.getUint16(11, true) || h;
      const c = document.createElement('canvas');
      c.width = w; c.height = h;
      c.getContext('2d').putImageData(new ImageData(new Uint8ClampedArray(buf, 13, w * h * 4), w, h), 0, 0);
      // A HiDPI cursor bitmap is shown at lw×lh logical px; image-set() tells the browser its density.
      const density = w / lw;
      canvas.style.cursor = density !== 1 ? `image-set(url("${c.toDataURL()}") ${density}x) ${hx} ${hy}, default` : `url(${c.toDataURL()}) ${hx} ${hy}, default`;
      if (density !== 1 && !canvas.style.cursor.includes('image-set')) { // no image-set() in cursor: resize the bitmap instead
        const s = document.createElement('canvas');
        s.width = lw; s.height = lh;
        s.getContext('2d').drawImage(c, 0, 0, lw, lh);
        canvas.style.cursor = `url(${s.toDataURL()}) ${hx} ${hy}, default`;
      }
      break;
    }
    case POINTER_LOCK:
      // A client locked the pointer (a game, say): lock the browser's too and send raw deltas.
      wantLock = dv.getUint8(1) !== 0;
      if (wantLock) requestLock();
      else if (document.pointerLockElement) document.exitPointerLock();
      break;
    case AUDIO:
      onAudio(buf);
      break;
    case WINDOWS:
      onWindows(JSON.parse(new TextDecoder().decode(new Uint8Array(buf, 1))));
      break;
    case CLIPBOARD:
      onClipboard(new TextDecoder().decode(new Uint8Array(buf, 1)));
      break;
    case VIDEO: {
      if (dropNext) { dropNext = false; return; } // debug: bw.dropNext() simulates a lost message
      received++;
      if (!decoder) return;
      const key = (dv.getUint8(1) & 1) !== 0, seq = dv.getUint16(2, true);
      if (key) { keyframes++; sinceKey = 0; } else sinceKey++;
      // A gap in seq means the server dropped frames for us; a delta after a gap can't be decoded.
      const gap = videoSeq >= 0 ? (seq - videoSeq - 1) & 0xffff : 0;
      videoSeq = seq;
      if (!awaitingKey) lost += gap; // while we wait for a keyframe we asked for, the skipped deltas are expected
      if (!key && (gap || awaitingKey || decoder.decodeQueueSize > 4)) {
        dropped++;
        if (!awaitingKey) { awaitingKey = true; send(REQUEST_KEYFRAME, 0); }
        return;
      }
      try {
        const pts = Number(dv.getBigUint64(4, true));
        if (!stats.hidden) inflight.set(pts, { at: performance.now(), output: 0 });
        decoder.decode(new EncodedVideoChunk({
          type: key ? 'key' : 'delta',
          timestamp: pts,
          data: new Uint8Array(buf, 12),
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

function updateStatus() {
  if (!stream) return;
  status.textContent = `${stream.codec} ${stream.width}×${stream.height} ${renderer} · ${fps} fps · ${mbps.toFixed(1)} Mbit/s · ${latencyMs.toFixed(0)} ms · ${lost} lost, ${dropped} dropped, ${decodeErrors} errors`;
}
// frame rate and bandwidth (video + audio) over the last second
setInterval(() => {
  fps = windowFrames; mbps = windowBytes * 8 / 1e6;
  windowFrames = 0; windowBytes = 0;
  updateStatus();
  renderStats();
}, 1000);

// Needs a user gesture: called on the lock event (usually right after the click that caused it) and retried on clicks.
function requestLock() {
  if (document.pointerLockElement) return;
  lockRequests++;
  canvas.requestPointerLock({ unadjustedMovement: true })?.catch?.(e => {
    lockError = String(e);
    canvas.requestPointerLock()?.catch?.(e2 => { lockError += ' / ' + e2; });
  });
}
document.onpointerlockchange = () => {
  if (!document.pointerLockElement && wantLock) { wantLock = false; send(POINTER_LOCK_LOST, 0); } // Escape etc.
};

// --- audio -----------------------------------------------------------------

// Opus packets decode with WebCodecs and are scheduled back to back on an AudioContext, a small
// lead ahead of the clock as a jitter buffer. Browsers keep the context suspended until a user
// gesture, so it's resumed from the first click or key.
let audioCtx, audioDecoder, nextPlay = 0, analyser, audioPackets = 0, audioDecoded = 0;
const AUDIO_LEAD = 0.06;
function onAudioData(data) {
  audioDecoded++;
  const now = audioCtx.currentTime;
  // Not running (no user gesture yet) or too far ahead (capture clock faster than ours): drop 20 ms.
  if (audioCtx.state !== 'running' || nextPlay > now + 3 * AUDIO_LEAD) { data.close(); return; }
  const ab = audioCtx.createBuffer(data.numberOfChannels, data.numberOfFrames, data.sampleRate);
  for (let ch = 0; ch < data.numberOfChannels; ch++) {
    const plane = new Float32Array(data.numberOfFrames);
    data.copyTo(plane, { planeIndex: ch, format: 'f32-planar' });
    ab.copyToChannel(plane, ch);
  }
  data.close();
  const src = audioCtx.createBufferSource();
  src.buffer = ab;
  src.connect(analyser);
  if (nextPlay < now + 0.01) { if (nextPlay) audioUnderruns++; nextPlay = now + AUDIO_LEAD; } // (re)start after a gap or underrun
  src.start(nextPlay);
  nextPlay += ab.duration;
}
// A decode error closes the decoder, so recovery is a fresh one (same as video).
function newAudioDecoder() {
  audioDecoder = new AudioDecoder({ output: onAudioData, error: e => { console.error(e); newAudioDecoder(); } });
  audioDecoder.configure({ codec: 'opus', sampleRate: 48000, numberOfChannels: 2 });
}
function onAudio(buf) {
  if (!audioCtx) {
    audioCtx = new AudioContext({ sampleRate: 48000 });
    analyser = audioCtx.createAnalyser(); // debug: lets window.bw() report what is playing
    analyser.connect(audioCtx.destination);
    newAudioDecoder();
  }
  audioPackets++;
  const dv = new DataView(buf);
  const seq = dv.getUint16(2, true);
  if (audioSeq >= 0 && seq !== ((audioSeq + 1) & 0xffff)) { lost += (seq - audioSeq - 1) & 0xffff; nextPlay = 0; } // a gap: restart the lead from now
  audioSeq = seq;
  audioDecoder.decode(new EncodedAudioChunk({ type: 'key', timestamp: Number(dv.getBigUint64(4, true)), data: new Uint8Array(buf, 12) }));
}
const resumeAudio = () => { if (audioCtx?.state === 'suspended') audioCtx.resume(); };

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
  if (e.type === 'pointerdown') { canvas.setPointerCapture(e.pointerId); resumeAudio(); flushClipboard(); if (wantLock) requestLock(); }
  canvas.onpointermove(e);
  send(BUTTON, 3, dv => { dv.setUint16(1, btn, true); dv.setUint8(3, e.type === 'pointerdown' ? 1 : 0); });
};
canvas.oncontextmenu = e => e.preventDefault();
canvas.addEventListener('wheel', e => {
  e.preventDefault();
  send(AXIS, 9, dv => { dv.setUint8(1, e.deltaMode); dv.setFloat32(2, e.deltaX, true); dv.setFloat32(6, e.deltaY, true); });
}, { passive: false });

// --- clipboard ---------------------------------------------------------------
// Desktop -> browser: text copied in an application arrives as CLIPBOARD; the browser clipboard takes it
// right away when the page may write, otherwise on the next gesture. Browser -> desktop: Ctrl+V (or
// Shift+Insert) is held back until the browser's paste event delivers the text, which goes to the
// desktop first, so the application pastes what the browser had.
let clipboardText = '', pendingClipboard = null, pendingPaste = null;
function onClipboard(text) {
  clipboardText = text;
  pendingClipboard = text;
  flushClipboard();
}
function flushClipboard() {
  if (pendingClipboard === null || !navigator.clipboard?.writeText) return;
  const text = pendingClipboard;
  navigator.clipboard.writeText(text).then(() => { if (pendingClipboard === text) pendingClipboard = null; }).catch(() => {});
}
const isPasteKey = e => (e.ctrlKey && e.code === 'KeyV') || (e.shiftKey && e.code === 'Insert');
function sendKey(code, pressed) { send(KEY, 3, dv => { dv.setUint16(1, code, true); dv.setUint8(3, pressed ? 1 : 0); }); }
function flushPaste() {
  if (!pendingPaste) return;
  const code = pendingPaste; pendingPaste = null;
  sendKey(code, true); sendKey(code, false);
}
document.onpaste = e => {
  if (e.target instanceof HTMLInputElement) return;
  e.preventDefault();
  const text = e.clipboardData?.getData('text/plain') ?? ''; // an empty browser clipboard clears the desktop's
  const b = new TextEncoder().encode(text);
  send(SET_CLIPBOARD, b.length, dv => new Uint8Array(dv.buffer, 1).set(b));
  flushPaste();
};

const onKey = e => {
  if (e.target instanceof HTMLInputElement || e.target.form) return; // typing in the page's own inputs or its paste form
  const code = KEYCODES[e.code];
  if (!code || e.repeat) return; // clients repeat keys themselves (wl_keyboard.repeat_info)
  if (e.type === 'keydown' && isPasteKey(e)) {
    // let the browser raise its paste event (no preventDefault); forward the key after it, or soon anyway
    pendingPaste = code;
    setTimeout(flushPaste, 150);
    resumeAudio();
    return;
  }
  if (e.type === 'keyup' && pendingPaste === code) return; // the deferred press+release pair covers it
  if (e.type === 'keyup') flushPaste(); // a modifier going up first would turn the deferred chord into a plain key
  e.preventDefault();
  resumeAudio();
  lastInput = performance.now();
  flushClipboard(); // a key press is a gesture too
  sendKey(code, e.type === 'keydown');
};
window.onkeydown = window.onkeyup = onKey;
const blur = () => { pendingPaste = null; send(BLUR, 0); }; // a deferred paste chord must not fire after its modifier was released
window.onblur = blur;
document.onvisibilitychange = () => { if (document.hidden) blur(); };

let resizeTimer;
window.onresize = () => { clearTimeout(resizeTimer); resizeTimer = setTimeout(sendResize, 150); };

// Fullscreen + Keyboard Lock: Ctrl+W, Ctrl+T, Alt+Tab… reach the Wayland clients instead of the browser.
document.getElementById('fs').onclick = async () => {
  await document.documentElement.requestFullscreen();
  navigator.keyboard?.lock?.();
};
document.onfullscreenchange = () => { if (!document.fullscreenElement) navigator.keyboard?.unlock?.(); };

function audioStats() {
  if (!analyser) return null;
  const bins = new Uint8Array(analyser.frequencyBinCount);
  analyser.getByteFrequencyData(bins);
  let peak = 0;
  for (let i = 1; i < bins.length; i++) if (bins[i] > bins[peak]) peak = i;
  return { packets: audioPackets, decoded: audioDecoded, state: audioCtx.state, peakHz: Math.round(peak * audioCtx.sampleRate / 2 / bins.length), level: bins[peak] };
}
window.bw = () => ({ frames, received, fps, mbps, audio: audioStats(), keyframes, decodeErrors, dropped, lost, videoSeq, audioSeq, clipboardText, connects, closes, latencyMs, renderer, stream, awaitingKey, lockRequests, lockError, locked: !!document.pointerLockElement, decoder: decoder?.state, queue: decoder?.decodeQueueSize });
Object.assign(window.bw, { dropNext: () => { dropNext = true; }, clipboard: { read: () => api('/api/clipboard').then(r => r.text()), write: text => api('/api/clipboard', { method: 'PUT', body: text }) }, windows: getWindows, control, snapshot, elements: elementsOf, activate: id => control({ id, op: 'activate' }), spawn: cmd => control({ op: 'spawn', cmd }) });
initDesktop(sendControl, () => stream && { w: stream.width / stream.scale, h: stream.height / stream.scale, scale: stream.scale }, () => send(BLUR, 0));
initRenderer().then(connect);
