// Run in Docker after npm run build. The real viewer engine uses a controlled desktop/API fixture.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { chromium } from 'playwright-core';

const server = createServer(async (req, res) => {
  try {
    const path = new URL(req.url, 'http://localhost').pathname;
    if (path.startsWith('/api/')) { res.setHeader('Content-Type', 'application/json'); return res.end('[]'); }
    const data = await readFile(new URL('../dist/' + (path === '/' ? 'index.html' : path.slice(1)), import.meta.url));
    res.setHeader('Content-Type', path.endsWith('.js') ? 'text/javascript' : path.endsWith('.css') ? 'text/css' : 'text/html');
    res.end(data);
  } catch { res.writeHead(404).end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const browser = await chromium.launch({ executablePath: '/usr/bin/chromium', args: ['--no-sandbox'] });
const errors = [];
try {
  const context = await browser.newContext();
  const fixture = () => {
    window.sent = []; window.elementRequests = []; window.holdElements = false; window.elementStatus = 200;
    window.WebSocket = class extends EventTarget {
      static OPEN = 1;
      readyState = 1;
      constructor() { super(); window.socket = this; queueMicrotask(() => this.onopen?.({})); }
      send(data) { window.sent.push([...new Uint8Array(data)]); }
      close() {}
    };
    const originalFetch = window.fetch;
    window.fetch = (url, options) => {
      if (!String(url).endsWith('/elements')) return originalFetch(url, options);
      const request = { url, status: window.elementStatus };
      window.elementRequests.push(request);
      const page = { request: window.elementRequests.length, level: 'full', elements: [{ role: 'button', x: 10, y: 10, w: 40, h: 20 }], error: 'Accessibility support is disabled.' };
      return Promise.resolve({ status: request.status, json: () => window.holdElements ? new Promise(resolve => { request.finish = () => resolve(page); }) : Promise.resolve(page) });
    };
    window.windowsFrame = (id = 1, title = 'Focused application') => {
      const windows = [{ id, title, app_id: 'overlay-test', focused: true, minimized: false, x: 20, y: 20, w: 400, h: 250, decoration: 0, geo_x: 0, geo_y: 0, updated_ms: 1, popups: [] }];
      const json = new TextEncoder().encode(JSON.stringify(windows)), packet = new Uint8Array(json.length + 1);
      packet[0] = 6; packet.set(json, 1); window.socket.onmessage({ data: packet.buffer });
    };
  };
  await context.addInitScript(fixture);
  const page = await context.newPage();
  page.on('pageerror', error => errors.push(error.message));
  const url = `http://127.0.0.1:${server.address().port}/#token=test`;
  const ready = async () => {
    await page.waitForFunction(() => !!window.bw?.store);
    await page.evaluate(() => { bw.store.set({ status: 'connected', role: 'controller', stream: { codec: 'vp8', width: 900, height: 600, scale: 1 } }); windowsFrame(); });
  };
  await page.goto(url); await ready();
  const trigger = page.getByRole('button', { name: 'Settings', exact: true });
  const panel = page.getByRole('dialog', { name: 'Settings', exact: true });
  const borders = panel.getByRole('checkbox', { name: 'Window borders', exact: true });
  const elements = panel.getByRole('checkbox', { name: 'UI elements', exact: true });
  assert.equal(await page.getByRole('button', { name: 'Window borders', exact: true }).count(), 0);
  assert.equal(await page.getByRole('button', { name: 'UI elements of the focused window', exact: true }).count(), 0);
  await trigger.click();
  assert(await borders.evaluate(el => el === document.activeElement));
  assert.equal(await borders.isChecked(), false); assert.equal(await elements.isChecked(), false);
  await page.evaluate(() => { window.sent = []; });
  await borders.press('Space');
  await page.waitForFunction(() => document.querySelectorAll('.box-border').length === 1);
  await borders.press('Tab');
  assert(await elements.evaluate(el => el === document.activeElement));
  await elements.press('Space');
  await page.waitForFunction(() => bw.store.get().elements?.status === 200 && document.querySelectorAll('.box-border').length === 2);
  assert.equal(await page.evaluate(() => localStorage.getItem('bw.borders')), '1');
  assert.equal(await page.evaluate(() => localStorage.getItem('bw.elements')), '1');
  assert.equal(await page.evaluate(() => sent.filter(p => [0x83, 0x84, 0x85, 0x86, 0x87, 0x91, 0x92].includes(p[0])).length), 0);
  await elements.press('Tab');
  assert(await borders.evaluate(el => el === document.activeElement));
  await borders.press('Shift+Tab');
  assert(await elements.evaluate(el => el === document.activeElement));
  await panel.getByRole('heading', { name: 'Overlays' }).click();
  await page.keyboard.press('Shift+Tab');
  assert(await elements.evaluate(el => el === document.activeElement));
  await panel.getByRole('heading', { name: 'Overlays' }).click();
  await page.keyboard.press('Tab');
  assert(await borders.evaluate(el => el === document.activeElement));
  await panel.getByRole('heading', { name: 'Overlays' }).click();
  await page.keyboard.press('a');
  assert.equal(await page.evaluate(() => sent.filter(p => [0x83, 0x84, 0x85, 0x86, 0x87, 0x91, 0x92].includes(p[0])).length), 0, 'panel heading keeps keyboard input local');
  await page.keyboard.press('Escape');
  assert.equal(await panel.count(), 0); assert(await trigger.evaluate(el => el === document.activeElement));
  assert(await page.evaluate(() => bw.store.get().elementsOn && bw.store.get().elements !== null));
  await trigger.click(); assert(await borders.isChecked() && await elements.isChecked());
  await page.getByRole('button', { name: 'Applications', exact: true }).click();
  assert.equal(await panel.count(), 0);
  await page.getByPlaceholder('Search applications…').waitFor();
  await trigger.click(); await panel.waitFor();
  assert.equal(await page.getByPlaceholder('Search applications…').count(), 0);
  await page.locator('header').click({ position: { x: 3, y: 3 } });
  assert.equal(await panel.count(), 0, 'header outside click dismisses settings');
  await trigger.click();
  await page.mouse.click(5, 500); assert.equal(await panel.count(), 0);
  await page.reload(); await ready(); await trigger.click();
  assert(await borders.isChecked() && await elements.isChecked());
  await elements.uncheck();
  await page.evaluate(() => { window.holdElements = true; });
  await elements.check();
  await page.waitForFunction(() => elementRequests.some(request => request.finish));
  await elements.uncheck();
  await page.evaluate(() => { for (const request of elementRequests) request.finish?.(); });
  await page.waitForTimeout(400);
  assert.equal(await page.evaluate(() => bw.store.get().elements), null, 'late response body cannot restore disabled elements');
  const beforeRestart = await page.evaluate(() => elementRequests.length);
  await elements.check();
  await page.waitForFunction(count => elementRequests.length > count && !!elementRequests.at(-1).finish, beforeRestart);
  const staleIndex = await page.evaluate(() => elementRequests.length - 1);
  await elements.uncheck();
  await page.evaluate(() => { window.holdElements = false; });
  await elements.check();
  await page.waitForFunction(index => bw.store.get().elements?.page.request > index + 1, staleIndex);
  const newest = await page.evaluate(() => bw.store.get().elements.page.request);
  await page.evaluate(index => elementRequests[index].finish(), staleIndex);
  await page.waitForTimeout(100);
  assert.equal(await page.evaluate(() => bw.store.get().elements.page.request), newest, 'off/on cannot let an old response replace the new tree');
  await elements.uncheck();
  const stopped = await page.evaluate(() => elementRequests.length);
  await page.evaluate(() => windowsFrame(2, 'Another focused application'));
  await page.waitForTimeout(400);
  assert.equal(await page.evaluate(() => elementRequests.length), stopped, 'disabled elements schedule no requests');
  await page.evaluate(() => { window.holdElements = false; window.elementStatus = 501; });
  await elements.check();
  await page.waitForFunction(() => bw.store.get().elements?.id === 2 && bw.store.get().elements?.status === 501);
  await page.getByText('Accessibility support is disabled.', { exact: true }).waitFor();
  await page.evaluate(() => { window.elementStatus = 200; windowsFrame(3, 'New focused application'); });
  await page.waitForFunction(() => bw.store.get().elements?.id === 3 && bw.store.get().elements.status === 200);

  await page.evaluate(() => bw.store.set({ role: 'viewer' }));
  await borders.uncheck(); await elements.uncheck();
  assert.equal(await borders.isDisabled(), false);
  await page.getByRole('button', { name: 'Fullscreen (browser shortcuts go to the desktop)', exact: true }).click();
  await page.waitForFunction(() => !!document.fullscreenElement);
  await panel.waitFor({ state: 'detached' });
  await page.evaluate(() => document.exitFullscreen());
  assert.equal(await panel.count(), 0);
  await page.setViewportSize({ width: 320, height: 640 });
  await trigger.click();
  const box = await panel.boundingBox(); assert(box.x >= 0 && box.x + box.width <= 320);
  assert((await borders.locator('..').boundingBox()).height >= 44);
  const phoneContext = await browser.newContext({ viewport: { width: 320, height: 640 }, hasTouch: true, isMobile: true });
  await phoneContext.addInitScript(fixture);
  const phone = await phoneContext.newPage();
  await phone.goto(url);
  await phone.waitForFunction(() => !!window.bw?.store);
  await phone.evaluate(() => bw.store.set({ status: 'connected', role: 'controller' }));
  await phone.getByRole('button', { name: 'Settings', exact: true }).tap();
  const phoneBorders = phone.getByRole('checkbox', { name: 'Window borders', exact: true });
  await phoneBorders.tap(); assert(await phoneBorders.isChecked());
  assert.equal(await borders.isChecked(), false, 'another viewer keeps its own overlays');
  assert.equal(await phone.evaluate(() => sent.filter(p => [0x83, 0x84, 0x85, 0x86, 0x87, 0x91, 0x92].includes(p[0])).length), 0);
  await phone.screenshot({ path: '/tmp/bw45-settings-narrow.png' });
  const popup = await context.newPage();
  await popup.goto(url.replace('/#', '/?window=1#'));
  await popup.waitForFunction(() => !!window.bw?.store);
  assert.equal(await popup.getByRole('button', { name: 'Settings', exact: true }).count(), 0);
  assert.equal(await popup.evaluate(() => bw.store.get().elementsOn), false);
  assert.deepEqual(errors, []);
  console.log('settings defaults/persistence, overlays, local keyboard, menus, late responses, unavailable support, read-only, fullscreen, narrow layout and window popup checks passed');
} finally {
  await browser.close();
  await new Promise(resolve => server.close(resolve));
}
