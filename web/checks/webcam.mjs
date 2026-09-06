// Run in the Docker image with BW_TEST_WEBCAM pointing to an idle, passed-through v4l2loopback device.
// Build the release binary first; the image must include its guvcview launcher.
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, open, readFile, rm } from 'node:fs/promises';
import { spawn, execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { chromium } from 'playwright-core';

const device = process.env.BW_TEST_WEBCAM;
assert(device, 'set BW_TEST_WEBCAM to an unused loopback device');
const probe = () => execFileSync('v4l2-ctl', ['-d', device, '--all'], { encoding: 'utf8' });
const idle = probe();
assert(idle.includes('v4l2 loopback') && !idle.includes('Video Capture'), 'requires an idle exclusive-caps loopback');
const root = await mkdtemp(tmpdir() + '/bw-webcam-');
await mkdir(root + '/runtime', { mode: 0o700 });
const log = await open(root + '/server.log', 'w');
const origin = 'http://127.0.0.1:8093';
const server = spawn('/src/target/release/browser-wayland', ['--webcam', device, '--no-audio', '--no-rtc', '--no-tls', '--render-node', 'none', '--codec', 'vp8', '--listen', '127.0.0.1:8093', '--socket-name', 'wayland-webcam'], {
  env: { ...process.env, HOME: root, XDG_CONFIG_HOME: root + '/config', XDG_RUNTIME_DIR: root + '/runtime', RUST_LOG: 'bw_server::api=debug' }, stdio: ['ignore', log.fd, log.fd],
});
const wait = async fn => {
  for (let i = 0; i < 200; i++) { if (await fn()) return; await new Promise(r => setTimeout(r, 100)); }
  throw new Error('timed out');
};
let browser;
try {
  await wait(async () => { try { return (await fetch(origin)).ok && !!await readFile(root + '/config/browser-wayland/token'); } catch { return false; } });
  const token = (await readFile(root + '/config/browser-wayland/token', 'utf8')).trim();
  browser = await chromium.launch({ env: { ...process.env, XDG_CONFIG_HOME: root + '/chromium' }, executablePath: '/usr/bin/chromium', args: ['--no-sandbox', '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream'] });
  const page = await browser.newPage();
  await page.goto(origin + '/#token=' + token);
  await page.waitForFunction(() => bw.store.get().camAvailable && !!bw.store.get().stream);
  await page.evaluate(() => bw.takeControl());
  await page.waitForFunction(() => bw.store.get().role === 'controller');
  await page.evaluate(() => bw.cam.start());
  await wait(() => { try { return probe().includes('Video Capture'); } catch { return false; } });
  const reader = spawn('v4l2-ctl', ['-d', device, '--stream-mmap', '--stream-count=3', '--stream-to=' + root + '/frames']);
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => { reader.kill(); reject(new Error('camera frames timed out')); }, 10000);
    reader.once('exit', code => { clearTimeout(timer); code === 0 ? resolve() : reject(new Error('capture reader failed')); });
  });
  const frames = await readFile(root + '/frames');
  assert.equal(frames.length, 1280 * 720 * 2 * 3, 'three decoded YUYV camera frames');
  assert(!frames.subarray(0, 1280 * 720 * 2).equals(frames.subarray(1280 * 720 * 4)), 'camera frames change');
  await page.evaluate(() => bw.launch('guvcview'));
  await page.waitForFunction(() => bw.store.get().windows.some(w => /guvcview/i.test(w.app_id)));
  const command = execFileSync('pgrep', ['-af', '/usr/bin/guvcview'], { encoding: 'utf8' });
  assert(command.includes('--device=' + device), 'menu passes the configured loopback');
  assert(!(await readFile(root + '/server.log', 'utf8')).includes('no video device'), 'guvcview opens the configured camera');
  console.log(JSON.stringify({ captureBytes: frames.length, width: 1280, height: 720, frames: 3, menuDeviceMatched: true }));
  for (const win of await page.evaluate(() => bw.store.get().windows)) await page.evaluate(id => bw.control({ id, op: 'close' }), win.id);
} finally {
  await browser?.close(); server.kill('SIGTERM');
  await new Promise(resolve => server.exitCode != null ? resolve() : server.once('exit', resolve));
  await log.close(); await rm(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
}
