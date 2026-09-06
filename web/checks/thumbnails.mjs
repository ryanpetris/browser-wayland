// Run in the Docker rig after building the release binary.
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, open, readFile, rm, writeFile } from 'node:fs/promises';
import { spawn, execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { chromium } from 'playwright-core';

const root = await mkdtemp(tmpdir() + '/bw-thumbnails-');
await mkdir(root + '/runtime', { mode: 0o700 });
const log = await open(root + '/server.log', 'w');
const origin = 'http://127.0.0.1:8093';

const wait = async fn => {
  for (let i = 0; i < 200; i++) { if (await fn()) return; await new Promise(r => setTimeout(r, 100)); }
  throw new Error('timed out');
};

let browser, server;
try {
const xml = '/usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml';
execFileSync('wayland-scanner', ['client-header', xml, root + '/xdg-shell-client-protocol.h']);
execFileSync('wayland-scanner', ['private-code', xml, root + '/xdg-shell-protocol.c']);
execFileSync('cc', ['-I' + root, '/src/crates/bw-compositor/checks/thumbnail-client.c', root + '/xdg-shell-protocol.c', '-lwayland-client', '-o', root + '/client']);
server = spawn('/src/target/release/browser-wayland', ['--no-audio', '--no-rtc', '--no-tls', '--render-node', 'none', '--codec', 'vp8', '--listen', '127.0.0.1:8093', '--socket-name', 'wayland-thumbnails'], {
  env: { ...process.env, HOME: root, XDG_CONFIG_HOME: root + '/config', XDG_RUNTIME_DIR: root + '/runtime', RUST_LOG: 'bw_server::api=debug' }, stdio: ['ignore', log.fd, log.fd],
});
  await wait(async () => { try { return (await fetch(origin)).ok && !!await readFile(root + '/config/browser-wayland/token'); } catch { return false; } });
  const token = (await readFile(root + '/config/browser-wayland/token', 'utf8')).trim(), headers = { Authorization: `Bearer ${token}` };
  browser = await chromium.launch({ env: { ...process.env, XDG_CONFIG_HOME: root + '/chromium' }, executablePath: '/usr/bin/chromium', args: ['--no-sandbox'] });
  const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
  await context.addInitScript(() => {
    localStorage.setItem('bw.sidebar', '1');
    const Observer = IntersectionObserver;
    window.IntersectionObserver = class extends Observer {
      constructor(callback, options) {
        super((entries, observer) => callback([{ isIntersecting: false, intersectionRect: { width: 0, height: 0 } }, ...entries], observer), options);
      }
    };
    const create = URL.createObjectURL, revoke = URL.revokeObjectURL;
    window.liveBlobUrls = new Set();
    URL.createObjectURL = blob => { const url = create(blob); liveBlobUrls.add(url); return url; };
    URL.revokeObjectURL = url => { liveBlobUrls.delete(url); revoke(url); };
  });
  const page = await context.newPage(), requests = [], errors = [];
  page.on('pageerror', error => errors.push(error.message));
  page.on('request', request => { if (request.url().includes('/snapshot.png')) requests.push({ at: Date.now(), url: request.url() }); });
  await page.goto(origin + '/#token=' + token);
  await page.waitForFunction(() => !!bw.store.get().stream);
  await page.evaluate(() => bw.takeControl());
  await page.evaluate(cmd => bw.spawn(cmd), root + '/client ' + root + '/command');
  await page.waitForFunction(() => bw.store.get().windows.some(w => w.app_id === 'thumbnail-surfaces'));
  const id = await page.evaluate(() => bw.store.get().windows.find(w => w.app_id === 'thumbnail-surfaces').id);
  const info = () => page.evaluate(id => bw.store.get().windows.find(w => w.id === id), id);
  const thumb = page.locator('[data-window-list] .group').filter({ hasText: 'Thumbnail surfaces' }).locator('.h-10 img');
  await thumb.waitFor();
  const source = () => thumb.getAttribute('src');
  const fresh = async different => {
    await wait(async () => {
      try {
      if (!await thumb.count()) return false;
      const url = await source();
      const rendered = await page.evaluate(async url => [...new Uint8Array(await (await fetch(url)).arrayBuffer())], url);
      const last = requests.filter(r => r.url.includes(`/windows/${id}/`)).at(-1);
      const response = await fetch(last.url, { headers });
      return response.ok && (!different || !Buffer.from(rendered).equals(different)) && Buffer.from(rendered).equals(Buffer.from(await response.arrayBuffer()));
      } catch { return false; }
    });
  };
  const command = async text => {
    const previousPng = Buffer.from(await thumb.evaluate(async img => [...new Uint8Array(await (await fetch(img.src)).arrayBuffer())]));
    const revision = (await info()).content_revision;
    await writeFile(root + '/command', text);
    await wait(async () => (await info()).content_revision > revision);
    await fresh(previousPng);
    console.log('fresh thumbnail:', text);
  };
  await fresh();
  const initial = requests.length;
  await page.waitForTimeout(3500);
  assert.equal(requests.length, initial, 'idle visible window does not refetch');
  for (const cmd of ['root ff208020', 'sub ffe02020', 'sub ff20e020', 'async ff2020e0', 'sub-off', 'popup', 'popup-sub ff20e0e0', 'popup-sub ffe0e020', 'popup-off']) await command(cmd);
  await page.waitForTimeout(3100);
  const beforeBurst = (await info()).content_revision;
  await command('burst');
  await page.waitForTimeout(3300);
  await fresh();
  assert((await info()).content_revision >= beforeBurst + 2, 'both burst commits advance the revision');
  await page.evaluate(id => bw.control({ id, op: 'minimize' }), id);
  await page.waitForTimeout(300);
  await command('root ff802020');
  await page.evaluate(id => bw.control({ id, op: 'activate' }), id);
  await page.evaluate(id => bw.control({ id, op: 'resize', w: 600, h: 240 }), id);
  await wait(async () => (await info()).w === 600);
  await fresh();
  const hidden = async (hide, show, label) => {
    await hide(); await page.waitForTimeout(300);
    const count = requests.length, image = await source();
    await writeFile(root + '/command', 'root ff' + Math.floor(Math.random() * 0xffffff).toString(16).padStart(6, '0'));
    await page.waitForTimeout(3400);
    assert.equal(requests.length, count, label + ' prevents requests');
    assert.equal(await source(), image, label + ' retains image');
    await show(); await fresh();
    console.log('visibility:', label);
  };
  await hidden(() => page.getByRole('button', { name: 'Windows and statistics', exact: true }).click(), () => page.getByRole('button', { name: 'Windows and statistics', exact: true }).click(), 'closed sidebar');
  await hidden(() => page.getByRole('button', { name: 'Statistics', exact: true }).click(), () => page.getByRole('button', { name: 'Windows', exact: true }).click(), 'other tab');
  await hidden(() => page.evaluate(() => { Object.defineProperty(document, 'hidden', { configurable: true, get: () => true }); document.dispatchEvent(new Event('visibilitychange')); }), () => page.evaluate(() => { delete document.hidden; document.dispatchEvent(new Event('visibilitychange')); }), 'hidden document');
  await hidden(() => page.evaluate(() => document.querySelector('canvas').parentElement.requestFullscreen()), () => page.evaluate(() => document.exitFullscreen()), 'fullscreen');
  await hidden(() => page.evaluate(() => {
    const list = document.querySelector('[data-window-list]'), spacer = document.createElement('div');
    spacer.id = 'thumbnail-scroll-space'; spacer.style.height = '2000px'; list.firstElementChild.append(spacer); list.scrollTop = 300;
  }), () => page.evaluate(() => { document.querySelector('[data-window-list]').scrollTop = 0; document.getElementById('thumbnail-scroll-space').remove(); }), 'scrolled row');
  const perWindow = requests.filter(r => r.url.includes(`/windows/${id}/`));
  assert(perWindow.every((r, i) => !i || r.at - perWindow[i - 1].at >= 2950), 'request starts respect three-second cooldown');
  const retained = await source();
  await page.getByRole('button', { name: 'Statistics', exact: true }).click();
  await page.getByRole('button', { name: 'Windows', exact: true }).click();
  await page.waitForTimeout(500);
  assert.equal(await source(), retained, 'unchanged reopening reuses URL');
  await page.evaluate(id => bw.control({ id, op: 'close' }), id);
  await page.waitForFunction(() => bw.store.get().windows.length === 0);
  await page.waitForTimeout(200);
  assert(!await page.evaluate(url => liveBlobUrls.has(url), retained), 'removed row revokes thumbnail URL');
  await page.evaluate(cmd => bw.spawn(cmd), 'env GDK_BACKEND=x11 python /src/crates/bw-compositor/checks/thumbnail-x11.py ' + root + '/x11-command');
  await page.waitForFunction(() => bw.store.get().windows.some(w => w.x11 && w.title === 'Thumbnail X11'));
  const x11 = await page.evaluate(() => bw.store.get().windows.find(w => w.x11 && w.title === 'Thumbnail X11'));
  const xThumb = page.locator('[data-window-list] .group').filter({ hasText: 'Thumbnail X11' }).locator('.h-10 img');
  await xThumb.waitFor();
  const xBefore = Buffer.from(await xThumb.evaluate(async img => [...new Uint8Array(await (await fetch(img.src)).arrayBuffer())]));
  await writeFile(root + '/x11-command', 'root ffe02020');
  await page.waitForFunction(({ id, revision }) => bw.store.get().windows.find(w => w.id === id).content_revision > revision, { id: x11.id, revision: x11.content_revision });
  await wait(async () => {
    try {
      const png = Buffer.from(await xThumb.evaluate(async img => [...new Uint8Array(await (await fetch(img.src)).arrayBuffer())]));
      const last = requests.filter(r => r.url.includes(`/windows/${x11.id}/`)).at(-1);
      const response = await fetch(last.url, { headers });
      return response.ok && !png.equals(xBefore) && png.equals(Buffer.from(await response.arrayBuffer()));
    } catch { return false; }
  });
  let resume, waiting = false;
  await page.route(`**/api/windows/${x11.id}/snapshot.png?*`, async route => {
    waiting = true; await new Promise(resolve => { resume = resolve; });
    await route.continue().catch(() => {});
  });
  await writeFile(root + '/x11-command', 'root ff20e0e0');
  await wait(() => waiting);
  const xUrl = await xThumb.getAttribute('src');
  await page.evaluate(() => bw.dispose());
  resume();
  await page.waitForTimeout(200);
  assert(!await page.evaluate(url => liveBlobUrls.has(url), xUrl), 'viewer disposal releases retained thumbnail');
  await fetch(origin + '/api/control', { method: 'POST', headers: { ...headers, 'Content-Type': 'application/json' }, body: JSON.stringify({ id: x11.id, op: 'close' }) });
  assert.deepEqual(errors, [], 'no errors when a row unmounts during capture');
  console.log('live Wayland/X11 content, surface lifecycle, minimized capture, resizing, visibility, cooldown and cleanup passed');
} finally {
  await browser?.close(); server?.kill('SIGTERM');
  if (server) await new Promise(resolve => server.exitCode != null ? resolve() : server.once('exit', resolve));
  await log.close(); await rm(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
}
