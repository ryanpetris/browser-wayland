// Run in the Docker rig after building the release binary.
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, open, readFile, rm } from 'node:fs/promises';
import { spawn, spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { chromium } from 'playwright-core';
import { MOTION_ABS } from '../src/protocol.js';

const binary = '/src/target/release/elsewhere';
for (const size of ['0x1080', '1921x1080', '1920x1', '8194x1080', '1920', 'ax1080', '1920x1080x2']) {
  const result = spawnSync(binary, ['--screen-size', size], { encoding: 'utf8' });
  assert.equal(result.status, 2, size);
  assert.match(result.stderr, /expected WIDTHxHEIGHT/);
}
const wait = async fn => {
  for (let i = 0; i < 200; i++) {
    if (await fn()) return;
    await new Promise(r => setTimeout(r, 100));
  }
  throw new Error('timed out');
};
for (const fixed of [true, false]) {
  const root = await mkdtemp(tmpdir() + '/elsewhere-fixed-size-');
  await mkdir(root + '/runtime', { mode: 0o700 });
  const log = await open(root + '/server.log', 'w');
  const origin = 'http://127.0.0.1:8093';
  const server = spawn(binary, ['--no-audio', '--no-rtc', '--no-tls', '--render-node', 'none', '--codec', 'vp8', '--listen', '127.0.0.1:8093', '--socket-name', 'wayland-fixed-size', ...(fixed ? ['--screen-size', '1280x720'] : [])], {
    env: { ...process.env, HOME: root, XDG_CONFIG_HOME: root + '/config', XDG_RUNTIME_DIR: root + '/runtime' }, stdio: ['ignore', log.fd, log.fd],
  });
  let browser;
  try {
    await wait(async () => { try { return (await fetch(origin)).ok && !!await readFile(root + '/config/elsewhere/token'); } catch { return false; } });
    const token = (await readFile(root + '/config/elsewhere/token', 'utf8')).trim();
    const nativeSize = async () => {
      const response = await fetch(origin + '/api/screenshot.png', { headers: { Authorization: `Bearer ${token}` } });
      assert.equal(response.status, 200);
      const png = Buffer.from(await response.arrayBuffer());
      return [png.readUInt32BE(16), png.readUInt32BE(20)];
    };
    assert.deepEqual(await nativeSize(), fixed ? [1280, 720] : [1920, 1080]);
    browser = await chromium.launch({ env: { ...process.env, XDG_CONFIG_HOME: root + '/chromium' }, executablePath: '/usr/bin/chromium', args: ['--no-sandbox'] });
    const pages = [];
    for (const [viewport, dpr] of [[{ width: 1000, height: 700 }, 1], [{ width: 700, height: 1000 }, 2]]) {
      const context = await browser.newContext({ viewport, deviceScaleFactor: dpr });
      await context.addInitScript(motion => {
        const send = WebSocket.prototype.send;
        WebSocket.prototype.send = function(data) {
          if (data instanceof ArrayBuffer && new Uint8Array(data)[0] === motion) {
            const view = new DataView(data);
            window.lastMotion = [view.getFloat32(1, true), view.getFloat32(5, true)];
          }
          return send.call(this, data);
        };
      }, MOTION_ABS);
      const page = await context.newPage();
      await page.goto(origin + '/#token=' + token);
      await page.waitForFunction(() => !!elsewhere.store.get().stream);
      pages.push(page);
    }
    const checkController = async page => {
      await page.waitForFunction(() => elsewhere.store.get().role === 'controller');
      await page.waitForTimeout(700);
      const stream = await page.evaluate(() => elsewhere.store.get().stream);
      assert.deepEqual(await nativeSize(), fixed ? [1280, 720] : [stream.width, stream.height]);
      if (fixed) {
        assert.deepEqual([stream.width, stream.height, stream.scale], [1280, 720, 1]);
        const rect = await page.locator('canvas.stage').boundingBox();
        assert(Math.abs(rect.width / rect.height - 1280 / 720) < 0.01);
        await page.mouse.move(rect.x + rect.width / 2, rect.y + rect.height / 2);
        const point = await page.evaluate(() => window.lastMotion);
        assert(point && Math.abs(point[0] - 640) < 2 && Math.abs(point[1] - 360) < 2, JSON.stringify(point));
      }
    };
    await checkController(pages[0]);
    const before = await nativeSize();
    await pages[0].setViewportSize({ width: 1500, height: 1000 });
    await checkController(pages[0]);
    if (!fixed) assert.notDeepEqual(await nativeSize(), before);
    await pages[1].evaluate(() => elsewhere.takeControl());
    await checkController(pages[1]);
    const id = await pages[0].evaluate(() => elsewhere.store.get().sessionId.toString());
    await pages[1].evaluate(id => elsewhere.handoff(id), id);
    await checkController(pages[0]);
    await pages[0].close();
    await checkController(pages[1]);
    console.log(`${fixed ? 'fixed' : 'automatic'} resolution: startup, resize, DPR, take control, handoff and disconnect passed`);
  } catch (error) {
    console.error(await readFile(root + '/server.log', 'utf8'));
    throw error;
  } finally {
    await browser?.close();
    server.kill('SIGTERM');
    await new Promise(resolve => server.exitCode != null ? resolve() : server.once('exit', resolve));
    await log.close();
    await rm(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
  }
}
