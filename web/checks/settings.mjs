// Run in Docker after npm run build. The real viewer engine uses a controlled desktop/API fixture.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { chromium } from 'playwright-core';

const server = createServer(async (req, res) => {
  try {
    const path = new URL(req.url, 'http://localhost').pathname;
    if (path.startsWith('/api/')) { res.setHeader('Content-Type', 'application/json'); return res.end(path === '/api/applications' ? JSON.stringify([{ id: 'local-test.desktop', name: 'Local Test', categories: ['Utility'] }]) : '[]'); }
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
    window.controlRequests = []; window.sent = []; window.elementRequests = []; window.holdElements = false; window.elementStatus = 200;
    window.WebSocket = class extends EventTarget {
      static OPEN = 1;
      readyState = 1;
      constructor() { super(); window.socket = this; queueMicrotask(() => this.onopen?.({})); }
      send(data) { window.sent.push([...new Uint8Array(data)]); }
      close() {}
    };
    const originalFetch = window.fetch;
    window.fetch = (url, options) => {
      if (String(url).endsWith('/api/control')) window.controlRequests.push(JSON.parse(options.body));
      if (!String(url).endsWith('/elements')) return originalFetch(url, options);
      const request = { url, status: window.elementStatus };
      window.elementRequests.push(request);
      const page = { request: window.elementRequests.length, level: 'full', elements: [{ role: 'button', x: 10, y: 10, w: 40, h: 20 }], error: 'Accessibility support is disabled.' };
      return Promise.resolve({ status: request.status, json: () => window.holdElements ? new Promise(resolve => { request.finish = () => resolve(page); }) : Promise.resolve(page) });
    };
    window.windowsFrame = (id = 1, title = 'Focused application') => {
      const windows = [{ id, title, app_id: 'overlay-test', focused: true, minimized: false, x: 20, y: 20, w: 400, h: 250, decoration: 0, geo_x: 0, geo_y: 0, updated_ms: 1, content_revision: 1, popups: [] }];
      const json = new TextEncoder().encode(JSON.stringify(windows)), packet = new Uint8Array(json.length + 1);
      packet[0] = 6; packet.set(json, 1); window.socket.onmessage({ data: packet.buffer });
    };
  };
  await context.addInitScript(fixture);
  const page = await context.newPage();
  page.on('pageerror', error => errors.push(error.message));
  const url = `http://127.0.0.1:${server.address().port}/#token=test`;
  const ready = async () => {
    await page.waitForFunction(() => !!window.elsewhere?.store);
    await page.evaluate(() => { elsewhere.store.set({ status: 'connected', role: 'controller', stream: { codec: 'vp8', width: 900, height: 600, scale: 1 } }); windowsFrame(); });
  };
  await page.goto(url.replace('#token=test', '?token=test'));
  await page.waitForFunction(() => elsewhere.store.get().status === 'no-token');
  assert.equal(await page.evaluate(() => sessionStorage.getItem('elsewhere.token')), null);
  assert.equal(new URL(page.url()).search, '');
  assert.equal(await page.evaluate(() => sent.length), 0);
  await page.goto(url.replace('/#', '/?token=ignored#')); await ready();
  assert.equal(await page.evaluate(() => sessionStorage.getItem('elsewhere.token')), 'test');
  assert.equal(new URL(page.url()).search + new URL(page.url()).hash, '');
  const trigger = page.getByRole('button', { name: 'Settings', exact: true });
  const panel = page.getByRole('dialog', { name: 'Settings', exact: true });
  const borders = panel.getByRole('checkbox', { name: 'Window borders', exact: true });
  const elements = panel.getByRole('checkbox', { name: 'UI elements', exact: true });
  const capture = panel.getByRole('checkbox', { name: 'Capture mouse on click', exact: true });
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
  await page.waitForFunction(() => elsewhere.store.get().elements?.status === 200 && document.querySelectorAll('.box-border').length === 2);
  assert.equal(await page.evaluate(() => localStorage.getItem('elsewhere.borders')), '1');
  assert.equal(await page.evaluate(() => localStorage.getItem('elsewhere.elements')), '1');
  assert.equal(await page.evaluate(() => sent.filter(p => [0x83, 0x84, 0x85, 0x86, 0x87, 0x91, 0x92].includes(p[0])).length), 0);
  await elements.press('Tab');
  assert(await capture.evaluate(el => el === document.activeElement));
  assert.equal(await capture.isChecked(), false);
  await capture.press('Tab');
  assert(await borders.evaluate(el => el === document.activeElement));
  await borders.press('Shift+Tab');
  assert(await capture.evaluate(el => el === document.activeElement));
  await panel.getByRole('heading', { name: 'Overlays' }).click();
  await page.keyboard.press('Shift+Tab');
  assert(await capture.evaluate(el => el === document.activeElement));
  await panel.getByRole('heading', { name: 'Overlays' }).click();
  await page.keyboard.press('Tab');
  assert(await borders.evaluate(el => el === document.activeElement));
  await panel.getByRole('heading', { name: 'Overlays' }).click();
  await page.keyboard.press('a');
  assert.equal(await page.evaluate(() => sent.filter(p => [0x83, 0x84, 0x85, 0x86, 0x87, 0x91, 0x92].includes(p[0])).length), 0, 'panel heading keeps keyboard input local');
  await page.keyboard.press('Escape');
  assert.equal(await panel.count(), 0); assert(await trigger.evaluate(el => el === document.activeElement));
  assert(await page.evaluate(() => elsewhere.store.get().elementsOn && elsewhere.store.get().elements !== null));
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
  const apps = page.getByRole('button', { name: 'Applications', exact: true });
  await apps.click();
  await page.getByRole('heading', { name: 'Accessories', exact: true }).click();
  await page.keyboard.press('a');
  await page.keyboard.press('Escape');
  assert(await apps.evaluate(el => el === document.activeElement));
  assert.equal(await page.getByPlaceholder('Search applications…').count(), 0);
  await apps.click();
  const search = page.getByPlaceholder('Search applications…');
  await search.fill('Local Test');
  await search.press('Enter');
  await search.waitFor({ state: 'detached' });
  assert.equal(await search.count(), 0);
  assert(await page.evaluate(() => controlRequests.some(p => p.op === 'launch' && p.app === 'local-test.desktop')), 'search Enter launches the selected application');
  await page.getByRole('button', { name: 'Quit Elsewhere', exact: true }).click();
  await page.getByRole('button', { name: 'Quit Elsewhere', exact: true }).last().click();
  await page.getByText('Quit Elsewhere? Every window closes with it, and the desktop is gone until it is started again.', { exact: true }).click();
  await page.keyboard.press('a');
  assert.equal(await page.evaluate(() => sent.filter(p => [0x83, 0x84, 0x85, 0x86, 0x87, 0x91, 0x92].includes(p[0])).length), 0, 'application and power menu keys stay local');
  await trigger.click(); await panel.waitFor();
  assert.equal(await page.getByRole('button', { name: 'Cancel', exact: true }).count(), 0);
  await page.keyboard.press('Escape');

  await apps.click();
  await page.mouse.click(5, 500);
  await page.evaluate(() => { window.sent = []; });
  await page.keyboard.press('a');
  assert.equal(await page.evaluate(() => sent.filter(p => p[0] === 0x87).length), 2, 'outside dismissal restores desktop typing');
  await apps.click();
  await search.fill('Local Test');
  await page.evaluate(() => { window.sent = []; });
  await search.press('Enter');
  await search.waitFor({ state: 'detached' });
  assert.equal(await page.evaluate(() => sent.filter(p => p[0] === 0x87).length), 0, 'launch Enter remains local through keyup');
  await page.keyboard.press('a');
  assert.equal(await page.evaluate(() => sent.filter(p => p[0] === 0x87).length), 2, 'typing after launch reaches desktop');
  for (const key of ['Enter', 'Space']) {
    await apps.click();
    await page.evaluate(() => { window.sent = []; window.controlRequests = []; });
    await page.getByRole('button', { name: 'Local Test', exact: true }).press(key);
    await search.waitFor({ state: 'detached' });
    assert.equal(await page.evaluate(() => controlRequests.filter(p => p.op === 'launch').length), 1, `result ${key} launches once`);
    assert.equal(await page.evaluate(() => sent.filter(p => p[0] === 0x87).length), 0, `result ${key} stays local`);
    await page.keyboard.press('a');
    assert.equal(await page.evaluate(() => sent.filter(p => p[0] === 0x87).length), 2, 'typing after result activation reaches desktop');
  }
  const power = page.locator('#power-toggle');
  for (const action of ['Cancel', 'Quit']) {
    await power.click();
    await page.getByRole('button', { name: 'Quit Elsewhere', exact: true }).last().click();
    await page.evaluate(() => { window.sent = []; });
    const button = page.getByRole('button', { name: action, exact: true });
    await button.press('Enter');
    await button.waitFor({ state: 'detached' });
    assert(await power.evaluate(el => el === document.activeElement));
    assert.equal(await page.evaluate(() => sent.filter(p => p[0] === 0x87).length), 0, `${action} Enter stays local`);
  }
  assert.equal(await page.evaluate(() => controlRequests.filter(p => p.op === 'quit').length), 1);
  await page.reload(); await ready(); await trigger.click();
  assert(await borders.isChecked() && await elements.isChecked());
  await elements.uncheck();
  await page.evaluate(() => { window.holdElements = true; });
  await elements.check();
  await page.waitForFunction(() => elementRequests.some(request => request.finish));
  await elements.uncheck();
  await page.evaluate(() => { for (const request of elementRequests) request.finish?.(); });
  await page.waitForTimeout(400);
  assert.equal(await page.evaluate(() => elsewhere.store.get().elements), null, 'late response body cannot restore disabled elements');
  const beforeRestart = await page.evaluate(() => elementRequests.length);
  await elements.check();
  await page.waitForFunction(count => elementRequests.length > count && !!elementRequests.at(-1).finish, beforeRestart);
  const staleIndex = await page.evaluate(() => elementRequests.length - 1);
  await elements.uncheck();
  await page.evaluate(() => { window.holdElements = false; });
  await elements.check();
  await page.waitForFunction(index => elsewhere.store.get().elements?.page.request > index + 1, staleIndex);
  const newest = await page.evaluate(() => elsewhere.store.get().elements.page.request);
  await page.evaluate(index => elementRequests[index].finish(), staleIndex);
  await page.waitForTimeout(100);
  assert.equal(await page.evaluate(() => elsewhere.store.get().elements.page.request), newest, 'off/on cannot let an old response replace the new tree');
  await elements.uncheck();
  const stopped = await page.evaluate(() => elementRequests.length);
  await page.evaluate(() => windowsFrame(2, 'Another focused application'));
  await page.waitForTimeout(400);
  assert.equal(await page.evaluate(() => elementRequests.length), stopped, 'disabled elements schedule no requests');
  await page.evaluate(() => { window.holdElements = false; window.elementStatus = 501; });
  await elements.check();
  await page.waitForFunction(() => elsewhere.store.get().elements?.id === 2 && elsewhere.store.get().elements?.status === 501);
  await page.getByText('Accessibility support is disabled.', { exact: true }).waitFor();
  await page.evaluate(() => { window.elementStatus = 200; windowsFrame(3, 'New focused application'); });
  await page.waitForFunction(() => elsewhere.store.get().elements?.id === 3 && elsewhere.store.get().elements.status === 200);

  await page.evaluate(() => elsewhere.store.set({ role: 'viewer' }));
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
  await phone.waitForFunction(() => !!window.elsewhere?.store);
  await phone.evaluate(() => elsewhere.store.set({ status: 'connected', role: 'controller' }));
  await phone.getByRole('button', { name: 'Settings', exact: true }).tap();
  const phoneBorders = phone.getByRole('checkbox', { name: 'Window borders', exact: true });
  await phoneBorders.tap(); assert(await phoneBorders.isChecked());
  assert.equal(await borders.isChecked(), false, 'another viewer keeps its own overlays');
  assert.equal(await phone.evaluate(() => sent.filter(p => [0x83, 0x84, 0x85, 0x86, 0x87, 0x91, 0x92].includes(p[0])).length), 0);
  await phone.screenshot({ path: '/tmp/elsewhere45-settings-narrow.png' });
  const popup = await context.newPage();
  await popup.goto(url.replace('/#', '/?window=1#'));
  await popup.waitForFunction(() => !!window.elsewhere?.store);
  assert.equal(await popup.getByRole('button', { name: 'Settings', exact: true }).count(), 0);
  assert.equal(await popup.evaluate(() => elsewhere.store.get().elementsOn), false);
  assert.deepEqual(errors, []);
  console.log('settings defaults/persistence, overlays, local keyboard, menus, late responses, unavailable support, read-only, fullscreen, narrow layout and window popup checks passed');
} finally {
  await browser.close();
  await new Promise(resolve => server.close(resolve));
}
