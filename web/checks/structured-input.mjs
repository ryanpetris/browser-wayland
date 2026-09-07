// Run in the Docker rig with Chromium, foot and the mounted release binary.
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, open, readFile, rm } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { tmpdir } from 'node:os';
import { chromium } from 'playwright-core';

const root = await mkdtemp(tmpdir() + '/elsewhere-input-');
await mkdir(root + '/runtime', { mode: 0o700 });
const log = await open(root + '/server.log', 'w');
const origin = 'http://127.0.0.1:8095';
const server = spawn(process.env.ELSEWHERE_BINARY || '/src/target/release/elsewhere', ['--no-audio', '--no-rtc', '--no-tls', '--render-node', 'none', '--codec', 'vp8', '--listen', '127.0.0.1:8095', '--socket-name', 'wayland-input'], {
  cwd: root, env: { ...process.env, HOME: root, XDG_CONFIG_HOME: root + '/config', XDG_RUNTIME_DIR: root + '/runtime' }, stdio: ['ignore', log.fd, log.fd],
});
const wait = async (fn, label = 'server readiness') => {
  for (let i = 0; i < 200; i++) { if (await fn()) return; await new Promise(r => setTimeout(r, 50)); }
  throw new Error(`${label} timed out`);
};
const contents = path => readFile(path, 'utf8').catch(() => null);
let browser;
try {
  await wait(async () => { try { return (await fetch(origin)).ok && !!await contents(root + '/config/elsewhere/token'); } catch { return false; } });
  const token = (await contents(root + '/config/elsewhere/token')).trim();
  const viewerToken = (await contents(root + '/config/elsewhere/viewer-token')).trim();
  browser = await chromium.launch({ executablePath: '/usr/bin/chromium', env: { ...process.env, XDG_CONFIG_HOME: root + '/chromium' }, args: ['--no-sandbox'] });
  const connect = async (token, id) => {
    const context = await browser.newContext();
    const page = await context.newPage();
    await page.goto(`${origin}/${id ? '?window=' + id : ''}#token=${token}`);
    await page.waitForFunction(() => window.elsewhere?.store.get().status === 'connected');
    await page.evaluate(() => elsewhere.setCaptureOnClick(false));
    return page;
  };
  const desktop = await connect(token);
  await desktop.evaluate(() => elsewhere.spawn('foot --app-id=input-check'));
  await desktop.waitForFunction(() => elsewhere.store.get().windows.some(w => w.app_id === 'input-check'));
  const id = await desktop.evaluate(() => elsewhere.store.get().windows.find(w => w.app_id === 'input-check').id);
  const windowPage = await connect(token, id);
  const observers = [await connect(viewerToken), await connect(viewerToken, id)];
  for (const [index, page] of [desktop, windowPage].entries()) {
    const output = `${root}/input-${index}`;
    const label = index ? 'window' : 'desktop';
    await page.evaluate(id => elsewhere.activate(id), id);
    // Raw input starts tee, independently of the structured messages under test.
    await page.locator('canvas').focus();
    await page.keyboard.type(`tee input-${index}`);
    await page.keyboard.press('Enter');
    await wait(async () => await contents(output) === '', `${label} raw input setup`);
    await page.evaluate(() => { elsewhere.type('first'); elsewhere.key('Return'); });
    await wait(async () => await contents(output) === 'first\n', `${label} structured text and Return`);
    for (const observer of observers) {
      await observer.evaluate(() => { elsewhere.type('forbidden'); elsewhere.key('Return'); elsewhere.key('ctrl+d'); });
      // Stream state acknowledges a later message on the same ordered socket.
      const quality = await observer.evaluate(() => elsewhere.store.get().streamState?.preset === 'low' ? 'high' : 'low');
      await observer.evaluate(quality => elsewhere.setChoice({ quality }), quality);
      await observer.waitForFunction(quality => elsewhere.store.get().streamState?.preset === quality, quality);
    }
    await page.evaluate(() => { elsewhere.type('second'); elsewhere.key('Return'); });
    await wait(async () => await contents(output) === 'first\nsecond\n', `${label} viewer-token rejection`);
    await page.evaluate(() => elsewhere.key('ctrl+d'));
    // A subsequent shell command proves the key chord reached the running app.
    const done = `${root}/done-${index}`;
    await page.evaluate(done => { elsewhere.type(`touch ${done}`); elsewhere.key('Return'); }, done);
    await wait(async () => await contents(done) === '', `${label} Ctrl+D and subsequent command`);
    assert.equal(await contents(output), 'first\nsecond\n', 'view-only structured input must not reach the app');
    console.log(`${index ? 'window' : 'desktop'} structured text, Return and Ctrl+D round trips; desktop and window viewer tokens rejected`);
  }
  const focused = `${root}/focused`;
  await desktop.evaluate(command => elsewhere.spawn(command), `foot --app-id=input-focus sh -c 'cat > ${focused}'`);
  await desktop.waitForFunction(() => elsewhere.store.get().windows.some(w => w.app_id === 'input-focus' && w.focused));
  await wait(async () => await contents(focused) === '', 'second terminal readiness');
  await windowPage.evaluate(() => { elsewhere.type('current focus'); elsewhere.key('Return'); });
  await wait(async () => await contents(focused) === 'current focus\n', 'window text follows current keyboard focus');
  console.log('window text follows current keyboard focus even when another window is streamed');
} catch (error) {
  console.error(error);
  console.error(await contents(root + '/server.log'));
  throw error;
} finally {
  await browser?.close();
  server.kill('SIGTERM');
  await new Promise(resolve => { if (server.exitCode !== null || server.signalCode !== null) resolve(); else server.once('exit', resolve); });
  await log.close();
  await rm(root, { recursive: true, force: true, maxRetries: 5 });
}
