// Run inside the Docker rig: node checks/rtc-endpoint.mjs.
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, open, readFile, rm } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { hostname, tmpdir } from 'node:os';
import { lookup } from 'node:dns/promises';
import { chromium } from 'playwright-core';

const root = await mkdtemp(tmpdir() + '/bw-rtc-endpoint-');
await mkdir(root + '/runtime', { mode: 0o700 });
const browser = await chromium.launch({ executablePath: '/usr/bin/chromium', args: ['--no-sandbox', `--unsafely-treat-insecure-origin-as-secure=http://${hostname()}:8097`] });
try {
  for (const [host, args, expectedPort] of [
    ['127.0.0.1', [], 8097],
    ['localhost', [], 8097],
    [hostname(), [], 8097],
    ['[::1]', [], 8097],
    ['localhost', ['--rtc-addr', '127.0.0.1', '--rtc-port', '8098'], 8098],
  ]) {
    const log = await open(root + '/desktop.log', 'w');
    const desktop = spawn('/src/target/release/browser-wayland', ['--no-audio', '--no-tls', '--render-node', 'none', '--codec', 'vp8', '--listen', '[::]:8097', ...args], {
      env: { ...process.env, XDG_CONFIG_HOME: root + '/config', XDG_RUNTIME_DIR: root + '/runtime' },
      stdio: ['ignore', log.fd, log.fd],
    });
    const exited = new Promise(resolve => desktop.once('exit', resolve));
    const context = await browser.newContext();
    try {
      const origin = `http://${host}:8097`;
      let ready = false;
      for (let i = 0; i < 200; i++) {
        try { if ((await fetch(origin)).ok) { ready = true; break; } } catch {}
        await new Promise(resolve => setTimeout(resolve, 50));
      }
      assert(ready, await readFile(root + '/desktop.log', 'utf8'));
      const token = (await readFile(root + '/config/browser-wayland/token', 'utf8')).trim();
      await context.addInitScript(() => {
        const Original = RTCPeerConnection;
        window.RTCPeerConnection = class extends Original {
          constructor(...args) { super(...args); window.testPeer = this; }
        };
      });
      const page = await context.newPage();
      await page.goto(`${origin}/#token=${token}`);
      await page.waitForFunction(() => window.bw?.store.get().rtcAvailable);
      await page.evaluate(() => bw.setTransport('webrtc'));
      await page.waitForFunction(() => bw.store.get().videoVia === 'webrtc', null, { timeout: 15000 });
      const remote = await page.evaluate(async () => {
        const stats = await testPeer.getStats();
        const pair = [...stats.values()].find(s => s.type === 'candidate-pair' && s.nominated);
        return stats.get(pair.remoteCandidateId);
      });
      assert.equal(remote.port, expectedPort);
      const expected = args.length ? ['127.0.0.1'] : (await lookup(host.replace(/^\[|\]$/g, ''), { all: true })).map(a => a.address);
      assert(expected.includes(remote.address), JSON.stringify(remote));
      console.log(`${host}, ${args.join(' ') || 'page endpoint'}: connected to ${remote.address}:${remote.port}`);
    } finally {
      await context.close();
      desktop.kill('SIGTERM');
      await exited;
      await log.close();
    }
  }
} finally {
  await browser.close();
  await rm(root, { recursive: true, force: true });
}
