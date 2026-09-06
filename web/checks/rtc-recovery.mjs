// Run in the Docker rig with the mounted release binary; optionally pass the Medium ceiling.
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, open, readFile, rm, writeFile } from 'node:fs/promises';
import { spawn, spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { chromium } from 'playwright-core';

const medium = 8000;
const chain = `ELSEWHERE_RTC_${process.pid}`;
const iptables = (...args) => { const r = spawnSync('iptables', args, { encoding: 'utf8' }); assert.equal(r.status, 0, r.stderr); };
iptables('-N', chain);
iptables('-I', 'OUTPUT', '-p', 'udp', '--dport', '8089', '-j', chain);
const block = () => iptables('-A', chain, '-j', 'DROP');
const unblock = () => iptables('-F', chain);
const root = await mkdtemp(tmpdir() + '/elsewhere-rtc-recovery-');
await mkdir(root + '/home'); await mkdir(root + '/runtime', { mode: 0o700 });
const log = await open(root + '/desktop.log', 'w');
const origin = 'http://127.0.0.1:8089';
const desktop = spawn('/src/target/release/elsewhere', ['--no-audio', '--no-tls', '--render-node', 'none', '--codec', 'vp8', '--bitrate', String(medium), '--listen', '127.0.0.1:8089', '--socket-name', 'wayland-rtc-check'], {
  env: { ...process.env, HOME: root + '/home', XDG_CONFIG_HOME: root + '/config', XDG_RUNTIME_DIR: root + '/runtime' },
  stdio: ['ignore', log.fd, log.fd],
});
const waitFor = async predicate => {
  for (let i = 0; i < 400; i++) { if (await predicate()) return; await new Promise(resolve => setTimeout(resolve, 50)); }
  throw new Error('condition timed out');
};
let browser;
try {
  await waitFor(async () => { try { await readFile(root + '/config/elsewhere/token'); return (await fetch(origin)).ok; } catch { return false; } });
  const token = (await readFile(root + '/config/elsewhere/token', 'utf8')).trim();
  browser = await chromium.launch({ executablePath: '/usr/bin/chromium', env: { ...process.env, XDG_CONFIG_HOME: root + '/browser-config' }, args: ['--no-sandbox', '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream'] });
  const errors = [];
  const connect = async id => {
    const context = await browser.newContext({ viewport: { width: 1600, height: 900 } });
    await context.addInitScript(() => {
      window.rtcTest = { reports: [], sent: [], peers: [], offers: [], socketFrames: 0, rtcFrames: 0, failOffers: 0, holdOffers: false, held: [], states: [] };
      rtcTest.tracks = [];
      const getMedia = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
      navigator.mediaDevices.getUserMedia = async options => { const media = await getMedia(options); rtcTest.tracks.push(...media.getTracks()); return media; };
      rtcTest.decodes = 0;
      const decode = VideoDecoder.prototype.decode;
      VideoDecoder.prototype.decode = function(...args) { rtcTest.decodes++; return decode.apply(this, args); };
      const OriginalPeer = window.RTCPeerConnection;
      window.RTCPeerConnection = class extends OriginalPeer {
        constructor(...args) { super(...args); rtcTest.peers.push(this); }
        createOffer(...args) {
          if (rtcTest.failOffers-- > 0) return Promise.reject(new Error('test offer failure'));
          if (rtcTest.holdOffers) return new Promise(resolve => rtcTest.held.push(() => super.createOffer(...args).then(resolve).catch(() => resolve({ type: 'offer', sdp: '' }))));
          return super.createOffer(...args);
        }
        createDataChannel(...args) {
          const channel = super.createDataChannel(...args); rtcTest.channel = channel;
          channel.addEventListener('open', e => { if (rtcTest.hideOpen) e.stopImmediatePropagation(); });
          channel.addEventListener('message', e => { rtcTest.rtcFrames++; if (rtcTest.hideOpen) e.stopImmediatePropagation(); });
          return channel;
        }
      };
      rtcTest.PeerClass = window.RTCPeerConnection;
      const OriginalSocket = window.WebSocket;
      window.WebSocket = class extends OriginalSocket {
        constructor(...args) {
          super(...args); rtcTest.socket = this;
          this.addEventListener('message', ({ data }) => {
            if (data instanceof ArrayBuffer && new Uint8Array(data)[0] === 2) {
              if (rtcTest.shiftMs !== undefined) { const dv = new DataView(data); dv.setBigUint64(4, dv.getBigUint64(4, true) + 10000000n - BigInt(rtcTest.shiftMs) * 1000n, true); }
              rtcTest.socketFrames++; rtcTest.lastVideo = data.slice(0);
            }
          });
        }
        send(data) {
          const bytes = new Uint8Array(data);
          rtcTest.sent.push(bytes[0]);
          if (bytes[0] === 0x96) rtcTest.reports.push(new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint16(1, true));
          if (bytes[0] === 0x95) {
            const v = JSON.parse(new TextDecoder().decode(bytes.subarray(1)));
            if (v.offer) {
              rtcTest.offers.push(v.g);
              if (rtcTest.rejectOffers > 0) { rtcTest.rejectOffers--; data = new Uint8Array([0x95, ...new TextEncoder().encode(JSON.stringify({ offer: 'invalid test SDP', g: v.g, endpoint: v.endpoint }))]); }
            }
          }
          super.send(data);
        }
      };
      window.rtcInject = v => rtcTest.socket.onmessage({ data: new Uint8Array([0x0d, ...new TextEncoder().encode(JSON.stringify(v))]).buffer });
    });
    const page = await context.newPage();
    page.on('pageerror', e => errors.push(e.message));
    await page.goto(`${origin}/${id ? '?window=' + id : ''}#token=${token}`);
    await page.waitForFunction(() => elsewhere.store.get().status === 'connected' && elsewhere.store.get().rtcAvailable);
    await page.evaluate(() => {
      elsewhere.setChoice({ quality: 'low' });
      elsewhere.store.subscribe(() => {
        const r = elsewhere.store.get().rtcRecovery;
        if (rtcTest.states.at(-1)?.state !== r.state) rtcTest.states.push({ ...r, at: Date.now() });
      });
    });
    return page;
  };
  const main = await connect();
  await main.evaluate(() => elsewhere.spawn('foot --app-id=rtc-check'));
  await main.waitForFunction(() => elsewhere.store.get().windows.some(w => w.app_id === 'rtc-check'));
  const id = await main.evaluate(() => elsewhere.store.get().windows.find(w => w.app_id === 'rtc-check').id);
  const popup = await connect(id);
  const active = page => page.waitForFunction(() => elsewhere.store.get().rtcRecovery.state === 'active');
  const waiting = page => page.waitForFunction(() => elsewhere.store.get().rtcRecovery.state === 'waiting');
  const keyframe = page => page.evaluate(() => rtcTest.socket.send(new Uint8Array([0x88])));
  const selected = async page => {
    assert.deepEqual(await page.evaluate(() => [elsewhere.store.get().transport, localStorage.getItem('elsewhere.transport'), elsewhere.store.get().choice.quality, elsewhere.store.get().streamState.ceiling_kbps]), ['webrtc', 'webrtc', 'low', 5000]);
    assert.equal(await page.evaluate(() => elsewhere.store.get().statsOn), false);
  };
  let pathCheck = 0;
  const socketPaths = async page => {
    const marker = `rtc-path-${++pathCheck}`, output = `/tmp/elsewhere-rtc-input-${process.pid}-${pathCheck}`;
    await page.evaluate(id => elsewhere.activate(id), id);
    await page.locator('canvas').focus();
    await page.keyboard.type(`touch ${output}`);
    await page.keyboard.press('Enter');
    await page.evaluate(({ marker }) => {
      const data = new DataTransfer(); data.setData('text/plain', marker);
      document.dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }));
    }, { marker, output });
    await waitFor(async () => { try { return (await readFile(output, 'utf8')) === ''; } catch { return false; } });
    await rm(output, { force: true });
    await page.waitForFunction(async marker => (await elsewhere.clipboard.read()) === marker, marker);
    assert(await page.evaluate(() => rtcTest.sent.includes(0x87) && rtcTest.sent.includes(0x8c)));
  };
  for (const page of [main, popup]) {
    // A failed offer falls back without changing either selection and automatically recovers.
    await page.evaluate(() => { rtcTest.failOffers = 1; elsewhere.setTransport('webrtc'); });
    await waiting(page); await selected(page);
    assert.match(await page.locator('[data-transport-status]').textContent(), /WebSocket.*retrying WebRTC/);
    await page.evaluate(() => { rtcTest.socketFrames = 0; }); await keyframe(page);
    await page.waitForFunction(() => rtcTest.socketFrames > 0);
    await active(page);
    await keyframe(page); await page.waitForFunction(() => rtcTest.rtcFrames > 0);
    await socketPaths(page);

    const accepted = await page.evaluate(() => rtcTest.offers.at(-1));
    await page.evaluate(g => {
      const send = v => Object.getPrototypeOf(WebSocket.prototype).send.call(rtcTest.socket, new Uint8Array([0x95, ...new TextEncoder().encode(JSON.stringify(v))]));
      send({close: true});
      for (const invalid of [null, '1', -1, 1.5, true]) send({close: true, g: invalid});
      send({close: true, g: g - 1});
      send({offer: 'invalid SDP', endpoint: {host: 'localhost', port: 8089}});
    }, accepted);
    const frames = await page.evaluate(() => rtcTest.rtcFrames);
    await keyframe(page);
    await page.waitForFunction(n => rtcTest.rtcFrames > n, frames);
    assert.equal(await page.evaluate(() => rtcTest.offers.at(-1)), accepted, 'malformed or stale messages cannot replace the active attempt');

    await page.evaluate(() => rtcTest.channel.close());
    await waiting(page); await selected(page); await active(page);
    const previous = await page.evaluate(() => rtcTest.offers.at(-1));
    await page.evaluate(() => rtcInject({ close: true, g: rtcTest.offers.at(-1) }));
    await waiting(page); await selected(page);
    assert.equal(await page.evaluate(() => elsewhere.store.get().rtcRecovery.reason), 'Server queue stalled');
    await page.getByRole('button', { name: 'Retry now', exact: true }).click();
    await active(page);
    const count = await page.evaluate(() => rtcTest.peers.length);
    await page.evaluate(g => { rtcInject({ close: true, g }); rtcInject({ answer: 'late invalid answer', g }); }, previous);
    await page.waitForTimeout(250);
    assert.equal(await page.evaluate(() => rtcTest.peers.length), count);
    assert.equal(await page.evaluate(() => elsewhere.store.get().rtcRecovery.state), 'active');

    // Loss near the healthy deadline must postpone the backoff reset.
    await page.waitForTimeout(6000);
    // Three separate seconds with dropped video trigger the shared loss fallback.
    for (let i = 0; i < 3; i++) {
      await page.evaluate(() => { elsewhere.dropNext(); rtcTest.socket.send(new Uint8Array([0x88])); });
      await page.waitForTimeout(150); await keyframe(page); await page.waitForTimeout(1800);
    }
    assert(await page.evaluate(() => rtcTest.states.some(r => r.reason === 'Repeated frame loss or stalls')));
    assert(await page.evaluate(() => { const r = rtcTest.states.findLast(r => r.reason === 'Repeated frame loss or stalls'); return r.nextAt - r.at > 1500; }), 'loss must postpone the healthy backoff reset');
    await active(page);
    // A quiet channel is healthy; silence itself does not trigger recovery.
    const peersBeforeIdle = await page.evaluate(() => rtcTest.peers.length);
    await page.waitForTimeout(11000);
    assert.equal(await page.evaluate(() => rtcTest.peers.length), peersBeforeIdle);
    await page.evaluate(() => rtcTest.channel.close()); await waiting(page);
    assert(await page.evaluate(() => { const r = elsewhere.store.get().rtcRecovery; return r.nextAt - Date.now() <= 1000; }));
    await page.evaluate(() => elsewhere.setTransport('websocket'));
    const stopped = await page.evaluate(() => rtcTest.peers.length);
    await page.waitForTimeout(1300);
    assert.equal(await page.evaluate(() => rtcTest.peers.length), stopped);

    await page.evaluate(() => { rtcTest.rejectOffers = 1; elsewhere.setTransport('webrtc'); });
    await waiting(page);
    assert.equal(await page.evaluate(() => elsewhere.store.get().rtcRecovery.reason), 'Offer rejected');
    await selected(page); await active(page);
    await page.evaluate(() => elsewhere.setTransport('websocket'));
    await page.evaluate(() => { rtcTest.lastVideo = null; }); await keyframe(page);
    await page.waitForFunction(() => rtcTest.lastVideo !== null);
    assert.equal(await page.evaluate(() => {
      const before = rtcTest.decodes;
      for (let n = 0; n < 100; n++) rtcTest.socket.onmessage({ data: rtcTest.lastVideo.slice(0) });
      return rtcTest.decodes - before;
    }), 0, 'duplicate frames never reach the decoder');

    // Selecting WebSocket during gathering cancels a late offer and all retries.
    await page.evaluate(() => { rtcTest.holdOffers = true; elsewhere.setTransport('webrtc'); });
    await page.waitForFunction(() => rtcTest.held.length > 0);
    const offers = await page.evaluate(() => rtcTest.offers.length);
    await page.evaluate(() => { elsewhere.setTransport('websocket'); rtcTest.holdOffers = false; rtcTest.held.splice(0).forEach(resolve => resolve()); });
    await page.waitForTimeout(250);
    assert.equal(await page.evaluate(() => rtcTest.offers.length), offers);
    assert(await page.evaluate(() => rtcTest.peers.every(p => p.connectionState === 'closed')));
    console.log(`${page === main ? 'desktop' : 'window'} offer/close/stall/loss recovery, idle health, late signaling and cancellation passed`);
  }

  await main.evaluate(() => elsewhere.spawn("foot --app-id=rtc-static sh -c 'printf static; sleep 1000'"));
  await main.waitForFunction(() => elsewhere.store.get().windows.some(w => w.app_id === 'rtc-static'));
  const staticId = await main.evaluate(() => elsewhere.store.get().windows.find(w => w.app_id === 'rtc-static').id);
  const staticWindow = await connect(staticId);
  for (const page of [main, staticWindow]) {
    await page.evaluate(() => { rtcTest.hideOpen = true; rtcTest.rtcFrames = 0; elsewhere.setTransport('webrtc'); });
    await page.waitForFunction(() => rtcTest.channel.readyState === 'open');
    await keyframe(page);
    await page.waitForFunction(() => rtcTest.rtcFrames > 0);
    assert.equal(await page.evaluate(() => elsewhere.store.get().videoVia), 'websocket');
    await page.waitForTimeout(1500);
    const before = await page.evaluate(() => rtcTest.socketFrames);
    await page.waitForTimeout(500);
    assert.equal(await page.evaluate(() => rtcTest.socketFrames), before, 'server claimed the static picture while browser still waited for open');
    await page.evaluate(() => { rtcTest.socketFrames = 0; elsewhere.setTransport('websocket'); rtcTest.hideOpen = false; });
    await page.waitForFunction(() => rtcTest.socketFrames > 0 && !elsewhere().awaitingKey);
  }
  await staticWindow.context().close();
  await main.evaluate(id => elsewhere.control({ id, op: 'close' }), staticId);
  console.log('pending browser attempts with server claims refresh static desktop/window pictures on close');

  // Block only this rig server's UDP port; socket input/video/signaling stay usable.
  block();
  await main.evaluate(() => elsewhere.setTransport('webrtc'));
  await waiting(main); await selected(main);
  await socketPaths(main);
  await main.evaluate(() => { rtcTest.socketFrames = 0; }); await keyframe(main);
  await main.waitForFunction(() => rtcTest.socketFrames > 0);
  const before = await main.evaluate(() => rtcTest.peers.length);
  await main.waitForFunction(n => rtcTest.peers.length > n, before);
  await waiting(main);
  assert.equal(await popup.evaluate(() => elsewhere.store.get().transport), 'websocket');
  unblock();
  await active(main); await keyframe(main);
  assert.equal(await main.evaluate(() => elsewhere.store.get().status), 'connected');
  await selected(main);
  console.log('real UDP block preserved socket video and automatic recovery after unblocking passed');

  await main.evaluate(() => elsewhere.spawn("foot --app-id=rtc-motion sh -c 'while :; do date +%s%N; sleep .02; done'"));
  await main.waitForFunction(() => elsewhere.store.get().windows.some(w => w.app_id === 'rtc-motion'));
  const motion = await main.evaluate(() => elsewhere.store.get().windows.find(w => w.app_id === 'rtc-motion').id);
  await main.waitForTimeout(500);
  const statesBeforeStall = await main.evaluate(() => rtcTest.states.length);
  block();
  await main.waitForFunction(n => rtcTest.states.slice(n).some(r => r.reason === 'Server queue stalled'), statesBeforeStall);
  unblock(); await active(main);
  await main.evaluate(id => elsewhere.control({ id, op: 'close' }), motion);
  console.log('real blocked-UDP server queue stall and recovery passed');

  // A new socket retries with fresh capabilities; callbacks from the old socket are inert.
  await main.evaluate(() => { rtcTest.oldSocket = rtcTest.socket; rtcTest.oldClose = rtcTest.socket.onclose; rtcTest.oldMessage = rtcTest.socket.onmessage; rtcTest.socket.close(); });
  await main.waitForFunction(() => rtcTest.socket !== rtcTest.oldSocket && elsewhere.store.get().status === 'connected');
  await active(main);
  await main.evaluate(() => { rtcTest.oldClose({ code: 4001, reason: 'late auth failure' }); rtcTest.oldMessage({ data: new Uint8Array([0x0d, ...new TextEncoder().encode(JSON.stringify({ close: true, g: rtcTest.offers.at(-1) }))]).buffer }); });
  assert.equal(await main.evaluate(() => elsewhere.store.get().rtcRecovery.state), 'active');
  await main.evaluate(async () => { await elsewhere.mic.start(); await elsewhere.cam.start(); });
  assert(await main.evaluate(() => rtcTest.tracks.length >= 2 && rtcTest.tracks.every(t => t.readyState === 'live')));
  await main.evaluate(() => elsewhere.dispose());
  assert(await main.evaluate(() => rtcTest.tracks.every(t => t.readyState === 'ended')));
  assert.deepEqual(await main.evaluate(() => [elsewhere.store.get().mic, elsewhere.store.get().cam, elsewhere.store.get().playback]), [false, false, null]);
  const disposed = await main.evaluate(() => rtcTest.peers.length);
  await main.waitForTimeout(1500);
  assert.equal(await main.evaluate(() => rtcTest.peers.length), disposed);
  assert(await main.evaluate(() => rtcTest.peers.every(p => p.connectionState === 'closed')));
  const congestion = await connect();
  await congestion.evaluate(() => {
    rtcTest.shiftMs = 0;
    rtcTest.frameTimer = setInterval(() => rtcTest.socket.send(new Uint8Array([0x88])), 100);
  });
  await congestion.waitForFunction(() => rtcTest.reports.length >= 2);
  const reportCount = await congestion.evaluate(() => rtcTest.reports.length);
  await congestion.evaluate(() => { rtcTest.shiftMs = 500; rtcTest.failOffers = 100; elsewhere.setTransport('webrtc'); });
  await congestion.waitForFunction(n => rtcTest.reports.slice(n).some(delay => delay > 300), reportCount);
  await congestion.evaluate(() => { clearInterval(rtcTest.frameTimer); elsewhere.setTransport('websocket'); });
  await congestion.context().close();
  console.log('failed attempts preserve the socket congestion baseline');
  const bounded = await connect();
  await bounded.clock.install();
  const initialKeyframes = await bounded.evaluate(() => rtcTest.sent.filter(t => t === 0x88).length);
  await bounded.evaluate(() => { rtcTest.failOffers = 100; elsewhere.setTransport('webrtc'); });
  for (let i = 0; i < 8; i++) {
    await bounded.waitForFunction(n => rtcTest.peers.length === n && elsewhere.store.get().rtcRecovery.state === 'waiting', i + 1);
    const delay = await bounded.evaluate(() => { const r = rtcTest.states.at(-1); return r.nextAt - r.at; });
    const ceiling = Math.min(30000, 1000 * 2 ** i);
    assert(delay >= ceiling * 0.8 - 5 && delay <= ceiling + 5, `retry ${i}: ${delay} ms`);
    assert(await bounded.evaluate(() => rtcTest.peers.every(p => p.connectionState === 'closed')));
    if (i < 7) await bounded.clock.fastForward(delay + 50);
  }
  assert.equal(await bounded.evaluate(() => rtcTest.sent.filter(t => t === 0x88).length), initialKeyframes, 'unclaimed failed attempts do not request keyframes');
  await bounded.evaluate(() => elsewhere.setTransport('websocket'));
  await bounded.clock.fastForward(60000);
  assert.equal(await bounded.evaluate(() => rtcTest.peers.length), 8);
  await bounded.evaluate(() => { rtcTest.failOffers = 0; rtcTest.holdOffers = true; elsewhere.setTransport('webrtc'); });
  await bounded.waitForFunction(() => rtcTest.held.length === 1);
  await bounded.clock.fastForward(10050);
  await waiting(bounded);
  assert.equal(await bounded.evaluate(() => elsewhere.store.get().rtcRecovery.reason), 'Connection attempt timed out');
  await bounded.evaluate(() => { elsewhere.setTransport('websocket'); rtcTest.held.splice(0).forEach(resolve => resolve()); });
  assert.equal(await bounded.evaluate(() => rtcTest.offers.length), 0);
  await bounded.evaluate(() => { window.RTCPeerConnection = undefined; elsewhere.setTransport('webrtc'); });
  assert.equal(await bounded.evaluate(() => elsewhere.store.get().rtcRecovery.state), 'unavailable');
  await bounded.clock.fastForward(60000);
  assert.equal(await bounded.evaluate(() => rtcTest.peers.length), 9);
  assert.equal(await bounded.evaluate(() => localStorage.getItem('elsewhere.transport')), 'webrtc');
  await bounded.context().close();
  await popup.evaluate(() => { rtcTest.holdOffers = true; elsewhere.setTransport('webrtc'); });
  await popup.waitForFunction(() => elsewhere.store.get().rtcRecovery.state === 'connecting');
  await popup.evaluate(id => elsewhere.control({ id, op: 'close' }), id);
  await popup.waitForFunction(() => elsewhere.store.get().status === 'gone');
  assert(await popup.evaluate(() => rtcTest.peers.every(p => p.connectionState === 'closed')));
  const revoked = await connect();
  await revoked.evaluate(() => { rtcTest.holdOffers = true; elsewhere.setTransport('webrtc'); });
  await fetch(origin + '/api/token/rotate', { method: 'POST', headers: { Authorization: 'Bearer ' + token } });
  await revoked.waitForFunction(() => elsewhere.store.get().status === 'unauthorized');
  const revokedCount = await revoked.evaluate(() => rtcTest.peers.length);
  await revoked.waitForTimeout(1500);
  assert.equal(await revoked.evaluate(() => rtcTest.peers.length), revokedCount);
  assert(await revoked.evaluate(() => rtcTest.peers.every(p => p.connectionState === 'closed')));
  await revoked.context().close();

  console.log('capped jittered backoff, explicit cancellation and unsupported browser passed');
  assert.deepEqual(errors, []);
  console.log('socket replacement, stale callbacks and viewer disposal passed');
} catch (error) {
  console.error(error);
  await writeFile('/tmp/elsewhere40-recovery-server-failure.log', await readFile(root + '/desktop.log'));
  throw error;
} finally {
  unblock(); iptables('-D', 'OUTPUT', '-p', 'udp', '--dport', '8089', '-j', chain); iptables('-X', chain);
  await browser?.close();
  desktop.kill('SIGTERM');
  await new Promise(resolve => { if (desktop.exitCode !== null) resolve(); else desktop.once('exit', resolve); });
  await log.close(); await rm(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
}
