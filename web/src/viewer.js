// The streaming engine: WebSocket, WebCodecs decode onto the canvas, input, clipboard, audio.
// React only draws the chrome around it (App.jsx) and reads what it publishes on `store`.
// Wire format mirrors crates/bw-server/src/protocol.rs.
import { KEYCODES } from './keycodes.js';
import { TOKEN, WINDOW, api, elementsOf, snapshot, control, uploadFile } from './api.js';
import { createStore } from './store.js';
import { CONFIG, VIDEO, CURSOR, POINTER_LOCK, AUDIO, WINDOWS, CLIPBOARD, ROLE, NOTICE, CLIPBOARD_DATA, NOTIFICATIONS, ROLES, AUTH, HELLO, RESIZE, MOTION_ABS, MOTION_REL, BUTTON, AXIS, KEY, REQUEST_KEYFRAME, BLUR, POINTER_LOCK_LOST, CONTROL, SET_CLIPBOARD, TAKE_CONTROL, NOTIFY, BTN } from './protocol.js';

const AUDIO_LEAD = 0.06;

export function createViewer() {
  const store = createStore({
    // 'no-token' | 'connecting' | 'connected' | 'retrying' | 'unauthorized' | 'gone'
    status: TOKEN ? 'connecting' : 'no-token',
    reason: '',
    // 'controller' drives the desktop and sizes it; 'participant' (a control token) watches and may take
    // control; 'viewer' (the viewer token) only watches
    role: null,
    stream: null, // the last Config: {streamId, codec, width, height, scale}
    renderer: '2d',
    windows: [],
    windowTitle: '', // window mode: the streamed window's title
    clipboardText: '',
    notice: '', // what the server just told us about our last action, shown for a few seconds
    notifications: [], // open desktop notifications, oldest first
    upload: null, // { name, index, count } while files dropped on the page go up
    filesRev: 0, // bumps when an upload finished, so the Files tab refreshes
    locked: false,
    elements: null, // the focused window's elements: {id, status, page}
    elementsOn: false,
    statsOn: false,
    stats: { fps: 0, mbps: 0, latencyMs: 0, lost: 0, dropped: 0, decodeErrors: 0, keyframes: 0, sinceKey: 0, frames: 0, received: 0, connects: 0, closes: [], audio: null, queue: 0, timings: null, lockRequests: 0, lockError: '' },
  });
  const state = () => store.get();

  let canvas = null, ctx = null, draw = null; // draw(frame) takes ownership of the frame and closes it
  let stage = { w: 0, h: 0 }; // CSS size of the area the canvas lives in
  let quitting = false; // this page asked the desktop to shut down: the socket's end is not a failure
  let noticeTimer;
  let ws, decoder, stream = null, awaitingKey = true;
  let frames = 0, received = 0, windowFrames = 0, windowBytes = 0, lastInput = 0, latencyMs = 0, lockRequests = 0, lockError = '', wantLock = false, connects = 0, closes = [], keyframes = 0, decodeErrors = 0, dropped = 0;
  let videoSeq = -1, audioSeq = -1, lost = 0, dropNext = false; // seq: last message seen per stream; lost: gaps in either
  let pendingFrame = null, rafId = 0;

  // --- stats ------------------------------------------------------------------
  // Per-stage timings of the last second (ms): receive→decoder output, output→paint, paint→paint.
  // Frames in flight are keyed by pts, and only tracked while the stats panel is shown.
  const stage_ = { decode: [], paint: [], interval: [] };
  const inflight = new Map(); // pts -> {at, output}
  let lastPaint = 0, sinceKey = 0, audioUnderruns = 0;
  const pct = (a, p) => (a.length ? a.slice().sort((x, y) => x - y)[Math.max(0, Math.ceil(a.length * p) - 1)] : null);

  // --- rendering ----------------------------------------------------------------
  // Paint from requestAnimationFrame only: drawing a WebGPU canvas outside the animation-frame cycle
  // makes Chromium present a cleared texture now and then. The 2D canvas is fine with immediate draws.
  function schedule(frame) {
    if (pendingFrame) { inflight.delete(pendingFrame.timestamp); pendingFrame.close(); }
    pendingFrame = frame;
    if (!rafId) rafId = requestAnimationFrame(() => { rafId = 0; const f = pendingFrame; pendingFrame = null; if (f) paintNow(f); });
  }
  function paintNow(frame) {
    const pts = frame.timestamp; // before draw() closes the frame
    try { draw(frame); frames++; windowFrames++; } catch (e) { console.error(e); frame.close(); }
    if (lastInput) { latencyMs = performance.now() - lastInput; lastInput = 0; } // input -> next painted frame
    if (!state().statsOn) { inflight.clear(); return; }
    const t = performance.now(), rec = inflight.get(pts);
    inflight.delete(pts);
    if (rec?.output) { stage_.decode.push(rec.output - rec.at); stage_.paint.push(t - rec.output); }
    if (lastPaint) stage_.interval.push(t - lastPaint);
    lastPaint = t;
  }

  // WebGPU imports the decoded frame as an external texture (zero-copy); opt-in via ?renderer=webgpu
  // because Chromium on Linux presents a blank frame now and then with it.
  async function initWebGPU() {
    const adapter = await navigator.gpu?.requestAdapter();
    if (!adapter) return null;
    const device = await adapter.requestDevice();
    device.lost.then(info => { console.warn('WebGPU device lost:', info.reason, info.message); location.reload(); });
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
      const pass = encoder.beginRenderPass({ colorAttachments: [{ view: context.getCurrentTexture().createView(), loadOp: 'clear', storeOp: 'store' }] });
      pass.setPipeline(pipeline);
      pass.setBindGroup(0, bindGroup);
      pass.draw(3);
      pass.end();
      device.queue.submit([encoder.finish()]);
      device.queue.onSubmittedWorkDone().then(() => frame.close(), () => frame.close());
    };
  }
  async function initRenderer() {
    if (new URLSearchParams(location.search).get('renderer') === 'webgpu') {
      try { draw = await initWebGPU(); } catch (e) { console.warn('WebGPU unavailable:', e); }
    }
    if (!draw) {
      ctx = canvas.getContext('2d');
      draw = frame => { try { ctx.drawImage(frame, 0, 0); } finally { frame.close(); } };
    }
    store.set({ renderer: draw && !ctx ? 'webgpu' : '2d' });
  }

  // --- connection -----------------------------------------------------------------
  function connect() {
    if (!TOKEN) { store.set({ status: 'no-token' }); return; }
    audioSeq = -1; // the server kept counting while we were away
    connects++;
    ws = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws${WINDOW ? '/window/' + WINDOW : ''}`);
    ws.binaryType = 'arraybuffer';
    ws.onopen = async () => {
      const t = new TextEncoder().encode(TOKEN);
      send(AUTH, t.length, dv => new Uint8Array(dv.buffer, 1).set(t));
      await sendHello();
      if (!WINDOW) sendResize(); // a window stream is the window's size
      else if (document.hasFocus()) sendControl({ id: +WINDOW, op: 'activate' }); // a popup is focused before its script runs
    };
    ws.onmessage = e => onMessage(e.data);
    ws.onclose = e => {
      closes.push(`${e.code}:${e.reason}`);
      if (e.code === 4001) {
        stream = null;
        forgetToken();
        if (document.fullscreenElement) document.exitFullscreen(); // the token dialog is outside the stage
        store.set({ status: 'unauthorized', reason: e.reason || 'wrong token', stream: null });
        return;
      }
      if (e.code === 4003) { store.set({ status: 'gone', reason: e.reason }); return; } // the window is gone
      if (quitting) { store.set({ status: 'quit', stream: null }); return; } // we asked for this
      store.set({ status: 'retrying' });
      setTimeout(connect, 1000);
    };
  }
  function forgetToken() { try { sessionStorage.removeItem('bw.token'); } catch {} }

  function send(type, size, fill) {
    if (ws?.readyState !== WebSocket.OPEN) return;
    const buf = new ArrayBuffer(1 + size), dv = new DataView(buf);
    dv.setUint8(0, type);
    fill?.(dv);
    ws.send(buf);
  }
  const sendText = (type, text) => { const b = new TextEncoder().encode(text); send(type, b.length, dv => new Uint8Array(dv.buffer, 1).set(b)); };
  /// A window action or spawn for the compositor, as JSON.
  const sendControl = obj => sendText(CONTROL, JSON.stringify(obj));

  // Which codec families this browser decodes, in hardware and at all (bit0 H.264, bit1 HEVC, bit2 VP9, bit3 AV1).
  async function sendHello() {
    const probes = ['avc1.640028', 'hev1.1.6.L120.90', 'vp09.00.40.08', 'av01.0.09M.08'];
    let hw = 0, sw = 0;
    for (const [i, codec] of probes.entries()) {
      const ok = async hardwareAcceleration => (await VideoDecoder.isConfigSupported({ codec, hardwareAcceleration }).catch(() => ({}))).supported;
      if (await ok('prefer-hardware')) hw |= 1 << i;
      if (await ok('no-preference')) sw |= 1 << i;
    }
    send(HELLO, 2, dv => { dv.setUint8(1, hw); dv.setUint8(2, sw); });
  }

  // The output takes the stage's size (CSS px × devicePixelRatio); in window mode a Resize resizes the window.
  function sendResize() {
    if (!stage.w || !stage.h) return;
    send(RESIZE, 8, dv => {
      dv.setUint16(1, Math.round(stage.w), true);
      dv.setUint16(3, Math.round(stage.h), true);
      dv.setFloat32(5, devicePixelRatio, true);
    });
  }
  let resizeTimer;
  function setStage(w, h) {
    stage = { w, h };
    fitCanvas();
    // a window tab's size is the window's: only a real change after the stream is up (not the popup
    // opening or settling) resizes the window
    if (WINDOW && (!stream || (Math.round(w) === Math.round(stream.width / stream.scale) && Math.round(h) === Math.round(stream.height / stream.scale)))) return;
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(sendResize, 150);
  }
  // The canvas at the stream's logical size, centred, scaled to the stage without distortion: for the
  // controller that is the stage itself; another viewer sees the controller's desktop letterboxed; a
  // window popup shows its window 1:1 unless smaller.
  function fitCanvas() {
    if (!stream || !canvas || !stage.w) return;
    const w = stream.width / stream.scale, h = stream.height / stream.scale;
    let k = Math.min(stage.w / w, stage.h / h);
    if (WINDOW) k = Math.min(1, k);
    canvas.style.width = `${w * k}px`;
    canvas.style.height = `${h * k}px`;
  }

  // A decode error closes the decoder for good, so recovery means a fresh one plus a keyframe.
  function newDecoder() {
    if (decoder && decoder.state !== 'closed') decoder.close();
    const d = new VideoDecoder({
      output: f => { const rec = inflight.get(f.timestamp); if (rec) rec.output = performance.now(); (ctx ? paintNow : schedule)(f); },
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
        if (pendingFrame) { pendingFrame.close(); pendingFrame = null; }
        canvas.width = stream.width;
        canvas.height = stream.height;
        fitCanvas();
        resync();
        store.set({ stream, status: 'connected' });
        fetchElements(); // the scale may have changed
        break;
      case CURSOR: {
        // The compositor doesn't draw the pointer; the browser does, with zero latency.
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
        if (wantLock && driving()) requestLock();
        else if (document.pointerLockElement) document.exitPointerLock();
        break;
      case AUDIO:
        onAudio(buf);
        break;
      case WINDOWS: {
        const list = JSON.parse(new TextDecoder().decode(new Uint8Array(buf, 1)));
        if (WINDOW) {
          const w = list.find(w => w.id === +WINDOW);
          if (w) { document.title = w.title || w.app_id; store.set({ windowTitle: w.title || w.app_id }); }
          break;
        }
        store.set({ windows: list });
        fetchElements();
        break;
      }
      case NOTIFICATIONS:
        store.set({ notifications: JSON.parse(new TextDecoder().decode(new Uint8Array(buf, 1))) });
        break;
      case CLIPBOARD_DATA:
        onClipboardData(new TextDecoder().decode(new Uint8Array(buf, 1)));
        break;
      case CLIPBOARD:
        onClipboard(new TextDecoder().decode(new Uint8Array(buf, 1)));
        break;
      case NOTICE:
        store.set({ notice: new TextDecoder().decode(new Uint8Array(buf, 1)) });
        clearTimeout(noticeTimer);
        noticeTimer = setTimeout(() => store.set({ notice: '' }), 6000);
        break;
      case ROLE: {
        const role = ROLES[dv.getUint8(1)] ?? 'viewer';
        store.set({ role });
        if (role !== 'controller' && document.pointerLockElement) document.exitPointerLock(); // only the controller's pointer is the desktop's
        break;
      }
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
          if (state().statsOn) inflight.set(pts, { at: performance.now(), output: 0 });
          decoder.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: pts, data: new Uint8Array(buf, 12), transfer: [buf] }));
          awaitingKey = false;
        } catch (e) {
          console.error(e);
          resync();
        }
      }
    }
  }

  // frame rate, bandwidth (video + audio) and timings over the last second
  setInterval(() => {
    const s = state();
    const timings = s.statsOn ? { decode: [pct(stage_.decode, .5), pct(stage_.decode, .95)], paint: [pct(stage_.paint, .5), pct(stage_.paint, .95)], interval: [pct(stage_.interval, .5), pct(stage_.interval, .95)] } : null;
    stage_.decode.length = stage_.paint.length = stage_.interval.length = 0;
    store.set({ stats: { fps: windowFrames, mbps: windowBytes * 8 / 1e6, latencyMs, lost, dropped, decodeErrors, keyframes, sinceKey, frames, received, connects, closes, audio: audioStats(), queue: decoder?.decodeQueueSize ?? 0, timings, lockRequests, lockError, underruns: audioUnderruns } });
    windowFrames = 0; windowBytes = 0;
  }, 1000);

  // --- pointer lock -----------------------------------------------------------------
  // Needs a user gesture: called on the lock event (usually right after the click that caused it) and retried on clicks.
  function requestLock() {
    if (document.pointerLockElement) return;
    lockRequests++;
    canvas.requestPointerLock({ unadjustedMovement: true })?.catch?.(e => {
      lockError = String(e);
      canvas.requestPointerLock()?.catch?.(e2 => { lockError += ' / ' + e2; });
    });
  }
  document.addEventListener('pointerlockchange', () => {
    store.set({ locked: !!document.pointerLockElement });
    if (!document.pointerLockElement && wantLock) { wantLock = false; send(POINTER_LOCK_LOST, 0); } // Escape etc.
  });

  // --- audio ---------------------------------------------------------------------------
  // Opus packets decode with WebCodecs and are scheduled back to back on an AudioContext, a small
  // lead ahead of the clock as a jitter buffer. Browsers keep the context suspended until a user
  // gesture, so it's resumed from the first click or key.
  let audioCtx, audioDecoder, nextPlay = 0, analyser, audioPackets = 0, audioDecoded = 0;
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
  function newAudioDecoder() {
    audioDecoder = new AudioDecoder({ output: onAudioData, error: e => { console.error(e); newAudioDecoder(); } });
    audioDecoder.configure({ codec: 'opus', sampleRate: 48000, numberOfChannels: 2 });
  }
  function onAudio(buf) {
    if (!audioCtx) {
      audioCtx = new AudioContext({ sampleRate: 48000 });
      analyser = audioCtx.createAnalyser(); // lets the stats report what is playing
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
  function audioStats() {
    if (!analyser) return null;
    const bins = new Uint8Array(analyser.frequencyBinCount);
    analyser.getByteFrequencyData(bins);
    let peak = 0;
    for (let i = 1; i < bins.length; i++) if (bins[i] > bins[peak]) peak = i;
    return { packets: audioPackets, decoded: audioDecoded, state: audioCtx.state, level: bins[peak], lead: (nextPlay - audioCtx.currentTime) * 1000 };
  }

  // --- input -----------------------------------------------------------------------------
  // Only the controller's pointer and keyboard are the desktop's (a window popup drives with any control token).
  const driving = () => WINDOW ? state().role !== 'viewer' : state().role === 'controller';
  // Stream logical px per canvas CSS px (1 except while a resize is in flight).
  const scaleX = () => (stream ? stream.width / stream.scale / canvas.clientWidth : 1);
  const scaleY = () => (stream ? stream.height / stream.scale / canvas.clientHeight : 1);
  function onPointerMove(e) {
    if (!driving()) return;
    if (document.pointerLockElement) send(MOTION_REL, 8, dv => { dv.setFloat32(1, e.movementX, true); dv.setFloat32(5, e.movementY, true); });
    else send(MOTION_ABS, 8, dv => { dv.setFloat32(1, e.offsetX * scaleX(), true); dv.setFloat32(5, e.offsetY * scaleY(), true); });
  }
  function onPointerButton(e) {
    const btn = BTN[e.button];
    if (btn === undefined) return;
    // the gesture counts for every session: audio and the browser clipboard need one
    if (e.type === 'pointerdown') { canvas.setPointerCapture(e.pointerId); canvas.focus({ preventScroll: true }); resumeAudio(); flushClipboard(); if (wantLock && driving()) requestLock(); }
    if (!driving()) return;
    onPointerMove(e);
    send(BUTTON, 3, dv => { dv.setUint16(1, btn, true); dv.setUint8(3, e.type === 'pointerdown' ? 1 : 0); });
  }
  function onWheel(e) {
    e.preventDefault();
    if (!driving()) return;
    send(AXIS, 9, dv => { dv.setUint8(1, e.deltaMode); dv.setFloat32(2, e.deltaX, true); dv.setFloat32(6, e.deltaY, true); });
  }

  // --- files ---------------------------------------------------------------------------------
  // Files dropped anywhere on the page go to the desktop's transfer folder, one after the other.
  async function uploadFiles(list) {
    const files = [...list];
    let saved = 0;
    for (const [index, file] of files.entries()) {
      store.set({ upload: { name: file.name, index: index + 1, count: files.length } });
      try { await uploadFile(file); saved++; } catch {}
    }
    store.set({ upload: null, filesRev: state().filesRev + 1, notice: saved ? `${saved} file${saved === 1 ? '' : 's'} saved to the desktop's transfer folder` : 'upload failed' });
    clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => store.set({ notice: '' }), 6000);
  }
  document.addEventListener('dragover', e => { if (e.dataTransfer?.types.includes('Files')) e.preventDefault(); });
  document.addEventListener('drop', e => {
    if (!e.dataTransfer?.files.length) return;
    e.preventDefault();
    if (state().role !== 'viewer') uploadFiles(e.dataTransfer.files);
    else { store.set({ notice: 'a view-only session cannot send files' }); clearTimeout(noticeTimer); noticeTimer = setTimeout(() => store.set({ notice: '' }), 6000); }
  });

  // --- clipboard ---------------------------------------------------------------------------
  // Desktop -> browser: text copied in an application arrives as CLIPBOARD, an image as CLIPBOARD_DATA
  // (its bytes are fetched from the API); the browser clipboard takes it right away when the page may
  // write, otherwise on the next gesture. Browser -> desktop: Ctrl+V (or Shift+Insert) is held back until
  // the browser's paste event delivers the text or image, which goes to the desktop first, so the
  // application pastes what the browser had.
  let pendingClipboard = null, pendingPaste = null, pasteTimer, clipboardGen = 0, swallowKeyup = null;
  function onClipboard(text) {
    clipboardGen++;
    pendingClipboard = text;
    store.set({ clipboardText: text });
    flushClipboard();
  }
  // the bytes are fetched once, now, so the write can happen inside a gesture (WebKit insists on that)
  function onClipboardData(mime) {
    const gen = ++clipboardGen;
    pendingClipboard = null;
    store.set({ clipboardText: '[image]' });
    api('/api/clipboard').then(r => r.blob()).then(blob => { if (gen === clipboardGen) { pendingClipboard = { mime, blob }; flushClipboard(); } }).catch(() => {});
  }
  function flushClipboard() {
    if (pendingClipboard === null || !navigator.clipboard?.writeText) return;
    const item = pendingClipboard;
    const done = () => { if (pendingClipboard === item) pendingClipboard = null; };
    if (typeof item === 'string') {
      navigator.clipboard.writeText(item).then(done).catch(() => {});
    } else if (window.ClipboardItem) {
      navigator.clipboard.write([new ClipboardItem({ [item.mime]: item.blob })]).then(done).catch(() => {});
    }
  }
  const isPasteKey = e => (e.ctrlKey && e.code === 'KeyV') || (e.shiftKey && e.code === 'Insert');
  const sendKey = (code, pressed) => send(KEY, 3, dv => { dv.setUint16(1, code, true); dv.setUint8(3, pressed ? 1 : 0); });
  function flushPaste() {
    clearTimeout(pasteTimer); // a stale timer must not fire the next chord early
    if (!pendingPaste) return;
    const code = pendingPaste; pendingPaste = null;
    sendKey(code, true); sendKey(code, false);
  }
  document.addEventListener('paste', e => {
    if (isFormField(e.target) || state().role === 'viewer') return;
    e.preventDefault();
    const image = [...(e.clipboardData?.items ?? [])].find(i => i.type === 'image/png')?.getAsFile();
    if (image) {
      // The user's chord is dropped (its modifier may go up before the upload is done); once the picture is
      // on the desktop clipboard the same chord is pressed through the API, and not at all if the upload failed.
      const chord = pendingPaste === KEYCODES.Insert ? 'shift+Insert' : 'ctrl+v';
      swallowKeyup = pendingPaste; pendingPaste = null; clearTimeout(pasteTimer);
      api('/api/clipboard', { method: 'PUT', headers: { 'Content-Type': 'image/png' }, body: image, signal: AbortSignal.timeout(5000) })
        .then(r => { if (r.ok) return api('/api/input', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ type: 'key', keys: chord }) }); })
        .catch(() => {});
      return;
    }
    sendText(SET_CLIPBOARD, e.clipboardData?.getData('text/plain') ?? ''); // an empty browser clipboard clears the desktop's
    flushPaste();
  });

  // Keys go to the desktop from anywhere in the page except its own controls (a focused button keeps Enter and Space).
  const isFormField = t => t instanceof HTMLInputElement || t instanceof HTMLTextAreaElement || t instanceof HTMLButtonElement;
  function onKey(e) {
    if (isFormField(e.target) || !driving()) return;
    const code = KEYCODES[e.code];
    if (!code || e.repeat) return; // clients repeat keys themselves (wl_keyboard.repeat_info)
    if (e.type === 'keydown' && isPasteKey(e)) {
      // let the browser raise its paste event (no preventDefault); forward the key after it, or soon anyway
      pendingPaste = code;
      pasteTimer = setTimeout(flushPaste, 150);
      resumeAudio();
      return;
    }
    if (e.type === 'keyup' && (pendingPaste === code || swallowKeyup === code)) { swallowKeyup = null; return; } // the deferred pair (or the API chord) covers it
    if (e.type === 'keyup') flushPaste(); // a modifier going up first would turn the deferred chord into a plain key
    e.preventDefault();
    resumeAudio();
    lastInput = performance.now();
    flushClipboard(); // a key press is a gesture too
    sendKey(code, e.type === 'keydown');
  }
  window.addEventListener('keydown', onKey);
  window.addEventListener('keyup', onKey);
  const blur = () => { pendingPaste = null; send(BLUR, 0); }; // a deferred paste chord must not fire after its modifier was released
  window.addEventListener('blur', blur);
  document.addEventListener('visibilitychange', () => { if (document.hidden) blur(); });
  if (WINDOW) window.addEventListener('focus', () => sendControl({ id: +WINDOW, op: 'activate' })); // keyboard focus follows the tab

  // --- fullscreen ------------------------------------------------------------------------------
  // Fullscreen (of the stage, the canvas's parent, so the chrome goes away and the output takes the
  // screen) + Keyboard Lock: Ctrl+W, Ctrl+T, Alt+Tab… reach the Wayland clients instead of the browser.
  async function fullscreen() {
    await canvas.parentElement.requestFullscreen();
    navigator.keyboard?.lock?.();
  }
  document.addEventListener('fullscreenchange', () => { if (!document.fullscreenElement) navigator.keyboard?.unlock?.(); });

  // --- elements --------------------------------------------------------------------------------
  // The focused window's elements, fetched again when anything the answer depends on changed: the focus,
  // the title, the content (updated_ms, whole seconds), the geometry, the open popups or the stream scale
  // (Chromium's web content is scaled by it). The 300 ms delay merges a burst of list updates into one request.
  let elementsKey = '', elementsTimer = 0;
  const focusedWindow = () => state().windows.find(w => w.focused && !w.minimized);
  function fetchElements() {
    const f = focusedWindow();
    const key = state().elementsOn && f ? `${f.id}/${f.title}/${f.updated_ms}/${f.w}x${f.h}+${f.geo_x}+${f.geo_y}@${stream?.scale}/${JSON.stringify(f.popups)}` : '';
    if (key === elementsKey) return;
    elementsKey = key;
    clearTimeout(elementsTimer);
    if (!key) { store.set({ elements: null }); return; }
    elementsTimer = setTimeout(async () => {
      const res = await api(`/api/windows/${f.id}/elements`).catch(() => null);
      if (elementsKey !== key) return; // superseded while in flight; the newer request is on its way
      if (!res) { elementsKey = ''; return; } // network failure: the next list update retries
      store.set({ elements: { id: f.id, status: res.status, page: await res.json().catch(() => ({})) } });
    }, 300);
  }

  // --- the canvas -------------------------------------------------------------------------------
  let attached = false;
  function attach(el) {
    if (attached || !el) return;
    attached = true;
    canvas = el;
    canvas.addEventListener('pointermove', onPointerMove);
    canvas.addEventListener('pointerdown', onPointerButton);
    canvas.addEventListener('pointerup', onPointerButton);
    canvas.addEventListener('contextmenu', e => e.preventDefault());
    canvas.addEventListener('wheel', onWheel, { passive: false });
    initRenderer().then(connect);
  }

  const viewer = {
    store,
    attach,
    setStage,
    fullscreen,
    control: sendControl,
    snapshot,
    elements: elementsOf,
    activate: id => sendControl({ id, op: 'activate' }),
    spawn: cmd => sendControl({ op: 'spawn', cmd }),
    launch: app => control({ op: 'launch', app }),
    quit: () => control({ op: 'quit' }).then(r => { quitting = r.ok; }), // only an accepted quit explains the socket's end
    setElementsOn(on) { store.set({ elementsOn: on }); fetchElements(); },
    setStatsOn(on) { store.set({ statsOn: on }); inflight.clear(); stage_.decode.length = stage_.paint.length = stage_.interval.length = 0; lastPaint = 0; },
    releaseInput: () => send(BLUR, 0), // a key held on the canvas must not stay held while a text field has the keyboard
    takeControl: () => send(TAKE_CONTROL, 0),
    uploadFiles,
    // a click ('default'), an action key, or nothing to dismiss; a session that can't act only hides it for itself
    notify(id, action) {
      if (state().role === 'viewer') store.set({ notifications: state().notifications.filter(n => n.id !== id) });
      else sendText(NOTIFY, JSON.stringify({ id, action }));
    },
    clipboard: { read: () => api('/api/clipboard').then(r => r.text()), write: text => api('/api/clipboard', { method: 'PUT', body: text }) },
    windows: () => state().windows,
    dropNext: () => { dropNext = true; },
  };
  // Console helpers, as documented: bw() for the numbers, bw.windows() and friends for the desktop.
  window.bw = () => ({ ...state().stats, stream, renderer: state().renderer, awaitingKey, locked: !!document.pointerLockElement, decoder: decoder?.state, clipboardText: state().clipboardText, videoSeq, audioSeq });
  Object.assign(window.bw, viewer);
  return viewer;
}
