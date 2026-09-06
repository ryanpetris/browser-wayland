// Run in the Docker rig with the mounted release binary; optionally pass the Medium ceiling.
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, open, readFile, rm, writeFile } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { tmpdir } from 'node:os';
import { chromium } from 'playwright-core';

const medium = Number(process.argv[2] ?? 8000);
const root = await mkdtemp(tmpdir() + '/elsewhere-quality-');
await mkdir(root + '/home'); await mkdir(root + '/runtime', { mode: 0o700 });
const log = await open(root + '/desktop.log', 'w');
const origin = 'http://127.0.0.1:8089';
const desktop = spawn('/src/target/release/elsewhere', ['--no-audio', '--no-tls', '--render-node', 'none', '--codec', 'vp8', '--bitrate', String(medium), '--listen', '127.0.0.1:8089', '--socket-name', 'wayland-quality'], {
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
  browser = await chromium.launch({ executablePath: '/usr/bin/chromium', env: { ...process.env, XDG_CONFIG_HOME: root + '/browser-config' }, args: ['--no-sandbox'] });
  const errors = [];
  const levels = [['very-low', 1, 2000], ['low', 2, 5000], ['medium', 3, medium], ['high', 4, 12000], ['max', 5, 25000]];
  const connect = async (windowId, saved) => {
    const context = await browser.newContext({ viewport: { width: 1600, height: 900 } });
    await context.addInitScript(saved => {
      if (saved !== null) localStorage.setItem('elsewhere.quality', saved);
      window.qualityStates = []; window.hellos = []; window.qualityOffers = []; window.qualitySocketFrames = 0; window.qualityRtcFrames = 0;
      const OriginalPeer = window.RTCPeerConnection;
      window.RTCPeerConnection = class extends OriginalPeer {
        createDataChannel(...args) {
          const channel = super.createDataChannel(...args); window.qualityChannel = channel;
          channel.addEventListener('message', () => window.qualityRtcFrames++);
          return channel;
        }
      };
      const Original = window.WebSocket;
      window.WebSocket = class extends Original {
        constructor(...args) {
          super(...args); window.qualitySocket = this;
          this.addEventListener('message', ({ data }) => {
            if (data instanceof ArrayBuffer && new Uint8Array(data)[0] === 0x02) qualitySocketFrames++;
            if (data instanceof ArrayBuffer && new Uint8Array(data)[0] === 0x0c) qualityStates.push(JSON.parse(new TextDecoder().decode(new Uint8Array(data, 1))));
          });
        }
        send(data) { const bytes = new Uint8Array(data); if (bytes[0] === 0x81) hellos.push([...bytes]); if (bytes[0] === 0x95) { const v = JSON.parse(new TextDecoder().decode(bytes.subarray(1))); if (v.offer) qualityOffers.push(v.g); } super.send(data); }
      };
    }, saved ?? null);
    const page = await context.newPage();
    page.on('pageerror', error => errors.push(error.message));
    await page.goto(`${origin}/${windowId ? '?window=' + windowId : ''}#token=${token}`);
    await page.waitForFunction(() => qualityStates.length && !!elsewhere.store.get().stream);
    return page;
  };
  const main = await connect(null, null);
  assert.equal(await main.getByTitle('Quality', { exact: true }).inputValue(), 'max');
  assert.deepEqual(await main.evaluate(() => [qualityStates[0].preset, qualityStates[0].ceiling_kbps, qualityStates[0].bitrate_kbps, hellos[0][4]]), ['max', 25000, 25000, 5]);
  await main.evaluate(() => elsewhere.spawn('foot --app-id=quality-check'));
  await main.waitForFunction(() => elsewhere.store.get().windows.some(w => w.app_id === 'quality-check'));
  const windowId = await main.evaluate(() => elsewhere.store.get().windows.find(w => w.app_id === 'quality-check').id);

  for (const id of [null, windowId]) {
    const page = id ? await connect(id, null) : main;
    const select = page.getByTitle('Quality', { exact: true });
    assert.equal(await select.inputValue(), 'max');
    assert.deepEqual(await select.locator('option').evaluateAll(options => options.map(o => o.value)), levels.map(([name]) => name));
    assert.deepEqual(await page.evaluate(() => [qualityStates[0].preset, qualityStates[0].bitrate_kbps, hellos[0][4]]), ['max', 25000, 5]);
    assert((await select.locator('option[value="medium"]').textContent()).includes(`${medium / 1000} Mbit/s`));
    for (const [name, wireId, ceiling] of levels) {
      await page.evaluate(() => { window.qualityStates = []; });
      await select.selectOption(name);
      await page.waitForFunction(name => qualityStates.some(s => s.preset === name), name);
      const state = await page.evaluate(name => qualityStates.find(s => s.preset === name), name);
      assert.equal(state.ceiling_kbps, ceiling); assert.equal(state.bitrate_kbps, ceiling);
      assert.equal(state.max_fps, ceiling < 3000 ? 30 : 0);
      assert.equal(state.medium_kbps, medium);
      assert.equal(await page.evaluate(() => localStorage.getItem('elsewhere.quality')), name);
      await page.reload();
      await page.waitForFunction(() => qualityStates.length > 0);
      assert.deepEqual(await page.evaluate(() => [elsewhere.store.get().choice.quality, hellos[0][4], qualityStates[0].bitrate_kbps]), [name, wireId, ceiling]);
    }
    await select.selectOption('max');
    await page.waitForFunction(() => elsewhere.store.get().streamState.preset === 'max');
    // A real browser-pressure report drives the existing controller; the selected level stays Max.
    await page.evaluate(() => {
      window.pressureTimer = setInterval(() => qualitySocket.send(new Uint8Array([0x96, 200, 0, 0, 0])), 100);
    });
    await page.waitForFunction(() => elsewhere.store.get().streamState.bitrate_kbps < 25000);
    await page.evaluate(() => clearInterval(window.pressureTimer));
    assert.equal(await select.inputValue(), 'max');
    assert((await select.locator('option:checked').textContent()).includes('up to 25 Mbit/s'));
    assert((await page.getByTitle('Current encoder target; actual network throughput depends on scene activity').textContent()).startsWith('Target '));
    await page.getByTitle(/^Measured video throughput:/).waitFor();
    await page.getByTitle('Video codec', { exact: true }).selectOption('auto');
    await page.waitForFunction(() => elsewhere.store.get().streamState.auto_codec);
    await page.evaluate(() => elsewhere.setChoice({ quality: 'low' }));
    await page.waitForFunction(() => elsewhere.store.get().streamState.preset === 'low');
    assert.equal(await select.inputValue(), 'low');
    assert.equal(await page.evaluate(() => localStorage.getItem('elsewhere.quality')), 'low');
    await page.evaluate(() => elsewhere.setTransport('webrtc'));
    await page.waitForFunction(() => elsewhere.store.get().videoVia === 'webrtc');
    await page.evaluate(() => qualitySocket.send(new Uint8Array([0x88])));
    await page.waitForFunction(() => qualityRtcFrames > 0);
    await page.evaluate(() => qualityChannel.close());
    await page.waitForFunction(() => elsewhere.store.get().videoVia === 'websocket');
    assert.equal(await select.inputValue(), 'low');
    assert.equal(await page.evaluate(() => elsewhere.store.get().streamState.ceiling_kbps), 5000);
    await page.evaluate(() => { window.qualityRtcFrames = 0; elsewhere.setTransport('webrtc'); });
    await page.waitForFunction(() => elsewhere.store.get().videoVia === 'webrtc');
    await page.evaluate(() => qualitySocket.send(new Uint8Array([0x88])));
    await page.waitForFunction(() => qualityRtcFrames > 0);
    assert.equal(await select.inputValue(), 'low');
    assert.equal(await page.evaluate(() => localStorage.getItem('elsewhere.quality')), 'low');
    assert.equal(await page.evaluate(() => elsewhere.store.get().streamState.ceiling_kbps), 5000);
    assert(await page.evaluate(() => qualityOffers.length >= 2 && qualityOffers.at(-2) !== qualityOffers.at(-1)));
    // A close from the first attempt must not release the successor's channel claim.
    await page.evaluate(() => {
      const body = new TextEncoder().encode(JSON.stringify({ close: true, g: qualityOffers.at(-2) }));
      qualitySocket.send(new Uint8Array([0x95, ...body]));
    });
    await page.waitForTimeout(200);
    await page.evaluate(() => { window.qualityRtcFrames = 0; qualitySocket.send(new Uint8Array([0x88])); });
    await page.waitForFunction(() => qualityRtcFrames > 0);
    // Explicit matching signaling releases the server claim even while the browser peer stays open.
    await page.evaluate(() => {
      window.qualitySocketFrames = 0;
      const body = new TextEncoder().encode(JSON.stringify({ close: true, g: qualityOffers.at(-1) }));
      qualitySocket.send(new Uint8Array([0x95, ...body]));
    });
    await page.waitForTimeout(200);
    await page.evaluate(() => qualitySocket.send(new Uint8Array([0x88])));
    await page.waitForFunction(() => qualitySocketFrames > 0);
    await page.evaluate(() => elsewhere.setTransport('websocket'));
    await page.evaluate(() => elsewhere.store.set({ streamState: { ...elsewhere.store.get().streamState, bitrate_kbps: 1562, max_fps: 30 } }));
    assert.equal(await page.getByTitle('Current encoder target; actual network throughput depends on scene activity').textContent(), 'Target 1.6 Mbit/s, 30 fps cap');

    for (const width of [1280, 800]) {
      await page.setViewportSize({ width, height: 768 });
      assert(await page.locator('footer').evaluate(el => el.scrollWidth <= el.clientWidth), `quality controls fit ${width}px`);
    }
    assert(await page.locator('footer').evaluate(footer => {
      const target = footer.querySelector('[title^="Current encoder target"]');
      const original = target.firstChild.data;
      const metrics = [...footer.children].slice(0, 4);
      const nodes = metrics.map(el => [...el.childNodes]);
      const icon = metrics[0].querySelector('svg').cloneNode(true);
      let stable = true;
      for (let width = 640; width <= 1280; width += 8) {
        footer.style.width = `${width}px`;
        let height;
        for (const values of [['9 fps', '9.9 Mbit/s', '9 ms', '0 · 0 · 0'], ['30 fps', '10.0 Mbit/s', '200 ms', '100 · 100 · 0'], ['60 fps', '100.0 Mbit/s', '2000 ms', '10000 · 10000 · 10']]) {
          metrics.forEach((el, i) => el.replaceChildren(...(i === 0 ? [icon.cloneNode(true)] : []), document.createTextNode(values[i])));
          for (const text of ['Target 25 Mbit/s', 'Target 12.5 Mbit/s', 'Target 1.6 Mbit/s, 30 fps cap', 'Target 4294967.3 Mbit/s']) {
            target.firstChild.data = text;
            height ??= footer.getBoundingClientRect().height;
            stable &&= footer.getBoundingClientRect().height === height && target.scrollWidth <= target.clientWidth;
          }
        }
      }
      target.firstChild.data = original; footer.style.width = '';
      metrics.forEach((el, i) => el.replaceChildren(...nodes[i]));
      return stable;
    }), 'live readouts fit without resizing the stage');
    await page.screenshot({ path: `/tmp/elsewhere41-${id ? 'window' : 'desktop'}-${medium}.png` });
    if (id) await page.context().close();
  }

  for (const id of [null, windowId]) {
    for (const saved of ['auto', 'invalid']) {
      const page = await connect(id, saved);
      assert.deepEqual(await page.evaluate(() => [elsewhere.store.get().choice.quality, hellos[0][4], qualityStates[0].bitrate_kbps]), ['max', 5, 25000]);
      await page.context().close();
    }
    for (const hello of [[0x81, 0, 16], [0x81, 0, 16, 0], [0x81, 0, 16, 0, 0], [0x81, 0, 16, 0, 255]]) {
      const ws = new WebSocket(origin.replace('http:', 'ws:') + '/ws' + (id ? '/window/' + id : ''));
      ws.binaryType = 'arraybuffer';
      let state;
      ws.addEventListener('message', ({ data }) => { if (new Uint8Array(data)[0] === 0x0c) state ??= JSON.parse(new TextDecoder().decode(new Uint8Array(data, 1))); });
      try {
        await new Promise((resolve, reject) => { ws.addEventListener('open', resolve, { once: true }); ws.addEventListener('error', reject, { once: true }); });
        ws.send(new Uint8Array([0x80, ...new TextEncoder().encode(token)])); ws.send(new Uint8Array(hello));
        if (hello.length < 5) {
          await waitFor(() => ws.readyState === WebSocket.CLOSED);
          assert.equal(state, undefined, 'short HELLO cannot start a stream');
        } else {
          await waitFor(() => state);
          assert.equal(state.preset, 'max'); assert.equal(state.bitrate_kbps, 25000); assert.equal(state.auto_codec, true);
        }
      } finally { ws.close(); }
    }
  }
  assert.deepEqual(errors, []);
  console.log(`desktop/window quality levels, defaults, persistence, invalid HELLO, live changes, pressure feedback, real RTC fallback/reconnect and Medium=${medium} passed`);
} catch (error) {
  await writeFile('/tmp/elsewhere41-quality-failure.log', await readFile(root + '/desktop.log'));
  throw error;
} finally {
  await browser?.close();
  desktop.kill('SIGTERM');
  await new Promise(resolve => { if (desktop.exitCode !== null) resolve(); else desktop.once('exit', resolve); });
  await log.close(); await rm(root, { recursive: true, force: true });
}
