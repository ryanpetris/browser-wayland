// Run in Docker after npm run build. Real browser pointer lock, controlled desktop messages.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { chromium } from 'playwright-core';
import { CONFIG, CURSOR, POINTER_LOCK, POINTER_LOCK_GAINED, POINTER_LOCK_LOST, ROLE, MOTION_ABS, MOTION_REL, BUTTON, AXIS, KEY, BLUR } from '../src/protocol.js';

const root = await mkdtemp('/tmp/elsewhere-capture-');
const server = createServer(async (req, res) => {
  try {
    const path = new URL(req.url, 'http://localhost').pathname;
    if (path.startsWith('/api/')) { res.setHeader('Content-Type', 'application/json'); return res.end('[]'); }
    res.setHeader('Content-Type', path.endsWith('.js') ? 'text/javascript' : path.endsWith('.css') ? 'text/css' : 'text/html');
    res.end(await readFile(new URL('../dist/' + (path === '/' ? 'index.html' : path.slice(1)), import.meta.url)));
  } catch { res.writeHead(404).end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const browser = await chromium.launch({ executablePath: '/usr/bin/chromium', env: { ...process.env, XDG_CONFIG_HOME: root }, args: ['--no-sandbox'] });
try {
  const context = await browser.newContext({ viewport: { width: 1200, height: 800 } });
  await context.addInitScript(() => {
    window.sent = [];
    window.WebSocket = class {
      static OPEN = 1;
      readyState = 1;
      constructor() { window.socket = this; queueMicrotask(() => this.onopen?.({})); }
      send(data) { sent.push([...new Uint8Array(data)]); }
      close() {}
    };
    window.packet = bytes => socket.onmessage({ data: new Uint8Array(bytes).buffer });
  });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  const ready = async () => {
    await page.waitForFunction(() => !!window.elsewhere?.store && !!window.socket);
    await page.evaluate(({ CONFIG, CURSOR, ROLE }) => {
      packet([ROLE, 2, 0]);
      packet([CONFIG, ...new TextEncoder().encode(JSON.stringify({ streamId: 1, codec: 'vp8', width: 1280, height: 720, scale: 1 }))]);
      const cursor = new Uint8Array(13 + 16 * 16 * 4), dv = new DataView(cursor.buffer);
      cursor[0] = CURSOR; dv.setUint16(1, 16, true); dv.setUint16(3, 16, true); cursor.fill(255, 13);
      packet(cursor);
    }, { CONFIG, CURSOR, ROLE });
  };
  await page.goto(`http://127.0.0.1:${server.address().port}/#token=test`);
  await ready();
  const canvas = page.locator('canvas.stage');
  const captured = async () => {
    try { await page.waitForFunction(() => elsewhere.store.get().locked && !!document.pointerLockElement, null, { timeout: 5000 }); }
    catch (error) { console.error(await page.evaluate(() => ({ role: elsewhere.store.get().role, status: elsewhere.store.get().status, stats: elsewhere.store.get().stats }))); throw error; }
  };
  const released = () => page.waitForFunction(() => !elsewhere.store.get().locked && !document.pointerLockElement);
  await canvas.click();
  assert.equal(await page.evaluate(() => !!document.pointerLockElement), false);
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  const toggle = page.getByRole('checkbox', { name: 'Capture mouse on click' });
  await toggle.check();
  assert.equal(await page.evaluate(() => localStorage.getItem('elsewhere.captureOnClick')), '1');
  await page.reload(); await ready();
  assert.equal(await page.evaluate(() => elsewhere.store.get().captureOnClick), true);
  const mousePackets = () => page.evaluate(types => sent.filter(p => types.includes(p[0])), [MOTION_ABS, MOTION_REL, BUTTON, AXIS]);
  await page.evaluate(() => { sent.length = 0; });
  await canvas.hover(); await page.mouse.wheel(0, 20);
  assert.deepEqual(await mousePackets(), [], 'unlocked mouse movement and wheel stay local');
  const before = await canvas.boundingBox();
  await canvas.click(); await captured();
  assert.equal(await page.evaluate(type => sent.filter(p => p[0] === type).length, POINTER_LOCK_GAINED), 1, 'successful capture notifies the desktop once');
  assert.equal((await mousePackets()).filter(p => p[0] === BUTTON).length, 0, 'capture click is not sent to the game');
  assert(await page.locator('footer [role=status]').isVisible());
  assert.equal(await page.locator('footer [role=status]').innerText(), 'Press Left Ctrl + Left Alt to release mouse');
  assert.deepEqual(await canvas.boundingBox(), before, 'capture hint does not resize the desktop');
  assert(await page.locator('[data-captured-cursor]').isVisible());
  const clicks = async () => {
    await page.evaluate(() => { sent.length = 0; });
    for (const button of ['left', 'right', 'middle', 'left']) {
      await page.mouse.down({ button });
      await page.mouse.up({ button });
    }
    assert.deepEqual(errors, [], 'captured clicks do not throw');
    assert.deepEqual((await mousePackets()).filter(p => p[0] === BUTTON), [
      [BUTTON, 0x10, 1, 1], [BUTTON, 0x10, 1, 0],
      [BUTTON, 0x11, 1, 1], [BUTTON, 0x11, 1, 0],
      [BUTTON, 0x12, 1, 1], [BUTTON, 0x12, 1, 0],
      [BUTTON, 0x10, 1, 1], [BUTTON, 0x10, 1, 0],
    ], 'every captured button press and release reaches the desktop');
  };
  await clicks();
  const move = async (x, y) => page.evaluate(([x, y]) => document.querySelector('canvas.stage').dispatchEvent(new PointerEvent('pointermove', { pointerType: 'mouse', movementX: x, movementY: y })), [x, y]);
  const lastMotion = async () => page.evaluate(({ MOTION_ABS, MOTION_REL }) => {
    const p = sent.filter(p => p[0] === MOTION_ABS || p[0] === MOTION_REL).at(-1), dv = new DataView(new Uint8Array(p).buffer);
    return [p[0], dv.getFloat32(1, true), dv.getFloat32(5, true)];
  }, { MOTION_ABS, MOTION_REL });
  await move(10000, 10000); assert.deepEqual(await lastMotion(), [MOTION_ABS, 1279, 719]);
  await move(10000, 10000); assert.deepEqual(await lastMotion(), [MOTION_ABS, 1279, 719]);
  await move(-10, -10); assert((await lastMotion())[1] < 1279, 'no accumulated movement beyond the edge');
  await move(-10000, -10000); assert.deepEqual(await lastMotion(), [MOTION_ABS, 0, 0]);
  await page.evaluate(type => packet([type, 1]), POINTER_LOCK);
  await move(12, -7); assert.deepEqual(await lastMotion(), [MOTION_REL, 12, -7]);
  assert.equal(await page.locator('[data-captured-cursor]').isVisible(), false);
  await clicks();
  await page.evaluate(type => packet([type, 0]), POINTER_LOCK);
  await captured(); assert(await page.locator('[data-captured-cursor]').isVisible());
  await page.keyboard.press('ControlLeft+AltRight'); await captured();
  await page.evaluate(() => { sent.length = 0; });
  await page.keyboard.press('ControlLeft+AltLeft'); await released();
  assert.deepEqual(await mousePackets(), [], 'release leaves the remote pointer in place');
  assert.deepEqual(await page.evaluate(type => sent.filter(p => p[0] === type), POINTER_LOCK_LOST), [[POINTER_LOCK_LOST]], 'release notifies the desktop even without an application lock acknowledgement');
  // Responses to capture can arrive after the user has already released it.
  const lateRequests = await page.evaluate(type => {
    const c = document.querySelector('canvas.stage'), request = c.requestPointerLock;
    let requests = 0;
    c.requestPointerLock = function (...args) { requests++; return request.apply(this, args); };
    packet([type, 1]); packet([type, 0]);
    c.requestPointerLock = request;
    return requests;
  }, POINTER_LOCK);
  assert.equal(lateRequests, 0, 'delayed application lock response does not request browser capture');
  await page.waitForTimeout(250);
  assert.equal(await page.evaluate(() => !!document.pointerLockElement), false, 'delayed application lock response does not recapture the mouse');
  assert.equal(await page.evaluate(type => sent.filter(p => p[0] === type).length, POINTER_LOCK_GAINED), 0);
  await canvas.hover(); await page.mouse.wheel(0, 20);
  assert.deepEqual(await mousePackets(), [], 'released mouse input stays local');
  assert.equal(await page.locator('footer [role=status]').count(), 0);
  await page.evaluate(type => packet([type, ...new Array(12).fill(0)]), CURSOR);
  assert.equal(await canvas.evaluate(c => getComputedStyle(c).cursor), 'default', 'released mouse stays visible when the desktop hides its cursor');
  await page.evaluate(() => elsewhere.setCaptureOnClick(false));
  await page.waitForFunction(() => getComputedStyle(document.querySelector('canvas.stage')).cursor === 'none');
  await page.evaluate(() => elsewhere.setCaptureOnClick(true));
  await page.waitForFunction(() => getComputedStyle(document.querySelector('canvas.stage')).cursor === 'default');
  // Programmatic exit avoids Chromium's cooldown after its built-in unlock gesture.
  await page.waitForTimeout(1300);
  await canvas.click(); await captured();
  await page.evaluate(() => elsewhere.setCaptureOnClick(false)); await released();
  await page.evaluate(() => elsewhere.setCaptureOnClick(true));
  await page.reload(); await ready();
  await canvas.click(); await captured();
  await page.evaluate(type => packet([type, 1, 0]), ROLE); await released();
  await canvas.click(); assert.equal(await page.evaluate(() => !!document.pointerLockElement), false, 'participants cannot capture');
  await page.reload(); await ready();
  await canvas.click(); await captured();
  await page.evaluate(() => socket.onclose({ code: 4003, reason: 'closed' })); await released();
  await page.reload(); await ready();
  await page.getByRole('button', { name: /fullscreen/i }).click();
  await page.waitForFunction(() => !!document.fullscreenElement);
  await canvas.click(); await captured();
  assert(await page.locator('.viewer-stage .mouse-capture-hint').isVisible());
  await page.evaluate(() => { sent.length = 0; });
  await page.evaluate(() => {
    const c = document.querySelector('canvas.stage');
    c.dispatchEvent(new KeyboardEvent('keydown', { code: 'ControlLeft', ctrlKey: true, bubbles: true }));
    c.dispatchEvent(new KeyboardEvent('keydown', { code: 'KeyV', ctrlKey: true, bubbles: true }));
    c.dispatchEvent(new KeyboardEvent('keydown', { code: 'AltLeft', ctrlKey: true, altKey: true, bubbles: true }));
  });
  await released(); await page.waitForTimeout(250);
  assert.equal(await page.evaluate(KEY => sent.some(p => p[0] === KEY && p[1] === 47 && p[3] === 1), KEY), false, 'release cancels deferred paste');
  await page.evaluate(() => {
    elsewhere.setTouchMouse(true);
    const c = document.querySelector('canvas.stage'), r = c.getBoundingClientRect();
    const capture = c.setPointerCapture; c.setPointerCapture = () => {};
    for (const type of ['pointerdown', 'pointerup']) c.dispatchEvent(new PointerEvent(type, { pointerType: 'touch', pointerId: 7, clientX: r.left + r.width / 4, clientY: r.top + r.height / 4 }));
    c.setPointerCapture = capture;
  });
  const touchMotion = await lastMotion();
  assert.deepEqual(touchMotion, [MOTION_ABS, 320, 180], 'touch-as-mouse still moves to the tap position while mouse capture is off');
  await page.evaluate(() => { sent.length = 0; });
  // Simulate a browser granting an earlier request after the viewer no longer wants it.
  await page.evaluate(() => {
    const c = document.querySelector('canvas.stage'), exit = document.exitPointerLock;
    window.lateExits = 0;
    Object.defineProperty(document, 'pointerLockElement', { configurable: true, get: () => c });
    document.exitPointerLock = () => { window.lateExits++; };
    for (const patch of [{ role: 'participant', status: 'connected', captureOnClick: true }, { role: 'controller', status: 'retrying', captureOnClick: true }, { role: 'controller', status: 'connected', captureOnClick: false }]) {
      elsewhere.store.set(patch); document.dispatchEvent(new Event('pointerlockchange'));
    }
    delete document.pointerLockElement; document.exitPointerLock = exit;
  });
  assert.equal(await page.evaluate(() => window.lateExits), 3);
  assert.equal(await page.evaluate(type => sent.filter(p => p[0] === type).length, POINTER_LOCK_GAINED), 0, 'rejected grants do not resume application locks');
  assert.deepEqual(errors, []);
  console.log('mouse capture: preference, real lock, edge clamping, cursor, game lock, release shortcut, role loss, disconnect and fullscreen passed');
} finally {
  await browser.close(); await new Promise(resolve => server.close(resolve));
  await rm(root, { recursive: true, force: true });
}
