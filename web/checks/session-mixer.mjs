// Run inside the Docker rig with the mounted release binary.
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, readdir, rm, open, writeFile } from 'node:fs/promises';
import { spawn, execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { chromium } from 'playwright-core';

const disabled = process.argv.includes('--no-audio');
const root = await mkdtemp(tmpdir() + '/bw-private-check-');
await mkdir(root + '/home'); await mkdir(root + '/runtime', { mode: 0o700 });
const log = await open(root + '/desktop.log', 'w');
const desktop = spawn('/src/target/release/browser-wayland', [...(disabled ? ['--no-audio'] : []), '--no-tls', '--no-rtc', '--render-node', 'none', '--listen', '127.0.0.1:8088', '--socket-name', 'wayland-private-check'], {
  env: { ...process.env, HOME: root + '/home', XDG_RUNTIME_DIR: root + '/runtime', XDG_CONFIG_HOME: root + '/config', PULSE_SINK: 'inherited-wrong-sink', PULSE_SOURCE: 'inherited-wrong-source', PIPEWIRE_NODE: '99999' },
  stdio: ['ignore', log.fd, log.fd],
});
let browser;
const waitFor = async predicate => {
  for (let i = 0; i < 400; i++) { if (await predicate()) return; await new Promise(r => setTimeout(r, 50)); }
  throw new Error('condition timed out');
};
try {
  await waitFor(async () => (await readFile(root + '/desktop.log', 'utf8')).includes('compositor ready'));
  await waitFor(async () => {
    try {
      await readFile(root + '/config/browser-wayland/token');
      return (await fetch('http://127.0.0.1:8088/')).ok;
    } catch { return false; }
  });
  const token = (await readFile(root + '/config/browser-wayland/token', 'utf8')).trim();
  browser = await chromium.launch({ executablePath: '/usr/bin/chromium', env: { ...process.env, HOME: root + '/home', XDG_CONFIG_HOME: root + '/browser-config', XDG_RUNTIME_DIR: root + '/runtime' }, args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required', '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream'] });
  const page = await browser.newPage();
  await page.addInitScript(() => {
    const Original = window.WebSocket;
    window.originalSocketSend = Original.prototype.send;
    window.WebSocket = class extends Original {
      constructor(...args) {
        super(...args); window.checkSocket = this; window.mixerLevelCount = 0;
        this.addEventListener('message', ({ data }) => { if (data instanceof ArrayBuffer && new Uint8Array(data)[0] === 0x10) window.mixerLevelCount++; });
      }
      send(data) {
        const bytes = new Uint8Array(data.buffer ?? data, data.byteOffset ?? 0, data.byteLength);
        if (window.holdMixerVolume && bytes[0] === 0x97 && JSON.parse(new TextDecoder().decode(bytes.subarray(1))).op === 'volume') {
          window.heldMixerVolume = data; return;
        }
        super.send(data);
      }
    };
  });
  await page.goto('http://127.0.0.1:8088/#token=' + token);
  await page.waitForFunction(() => window.bw?.store.get().status === 'connected');
  if (disabled) {
    await page.getByRole('button', { name: 'Session audio mixer', exact: true }).click();
    const panel = page.getByRole('region', { name: 'Session audio mixer', exact: true });
    await panel.getByText('Session audio is unavailable.', { exact: true }).waitFor();
    assert.equal(await panel.getByRole('slider').count(), 0);
    assert.equal(await page.evaluate(() => bw.store.get().mic), false);
    console.log('disabled audio exposes an unavailable mixer without stale controls');
  } else {
  assert.equal(await page.evaluate(() => bw.store.get().audioAvailable), true);
  await page.waitForFunction(() => bw.store.get().mixer.available);
  await page.getByRole('button', { name: 'Session audio mixer', exact: true }).click();
  const panel = page.getByRole('region', { name: 'Session audio mixer', exact: true });
  await panel.waitFor();
  assert.equal(await page.evaluate(() => bw.store.get().mic), false);
  await page.evaluate(() => bw.spawn('gst-launch-1.0 -q audiotestsrc is-live=true freq=440 volume=0.1 ! audioconvert ! audio/x-raw,format=S16LE,rate=48000,channels=2 ! pipewiresink sync=false stream-properties=properties,node.name=MixerBrowserTest,node.description=MixerBrowserTest,media.name=MixerBrowserTest,application.name=BrowserTest'));
  await page.waitForFunction(() => {
    const node = bw.store.get().mixer.nodes.find(n => n.name === 'MixerBrowserTest');
    return node?.meter_active && bw.store.get().mixerLevels[node.id] > .09;
  });
  const row = panel.getByRole('group', { name: /MixerBrowserTest/ });
  await row.getByRole('slider').fill('50');
  await page.waitForFunction(() => {
    const state = bw.store.get(), node = state.mixer.nodes.find(n => n.name === 'MixerBrowserTest');
    return Math.abs(node.volume - 50) < .1 && Math.abs(state.mixerLevels[node.id] - .0125) < .002;
  });
  await page.screenshot({ path: '/tmp/bw47-mixer-ui.png', fullPage: true });
  console.log('mixer panel renders; live native levels and authoritative volume passed');
  const nativeId = await page.evaluate(() => bw.store.get().mixer.nodes.find(n => n.name === 'MixerBrowserTest').id);
  const connect = async key => {
    const next = await browser.newPage();
    await next.addInitScript(() => {
      const Original = window.WebSocket;
      window.WebSocket = class extends Original { constructor(...args) { super(...args); window.checkSocket = this; } };
    });
    await next.goto('http://127.0.0.1:8088/#token=' + key);
    await next.waitForFunction(() => window.bw?.store.get().status === 'connected' && bw.store.get().mixer.available);
    await next.getByRole('button', { name: 'Session audio mixer', exact: true }).click();
    return next;
  };
  const raw = (target, command) => target.evaluate(command => {
    const json = new TextEncoder().encode(JSON.stringify(command));
    const bytes = new Uint8Array(json.length + 1); bytes[0] = 0x97; bytes.set(json, 1);
    checkSocket.send(bytes);
  }, command);
  const observer = await connect((await readFile(root + '/config/browser-wayland/viewer-token', 'utf8')).trim());
  const participant = await connect(token);
  assert.equal(await observer.evaluate(() => bw.store.get().role), 'viewer');
  assert.equal(await participant.evaluate(() => bw.store.get().role), 'participant');
  for (const target of [observer, participant]) {
    assert(await target.getByRole('group', { name: /MixerBrowserTest/ }).getByRole('slider').isDisabled());
    await raw(target, { op: 'volume', id: nativeId, value: 0 });
    await target.waitForFunction(() => bw.store.get().mixerError.includes('controlling'));
  }
  assert.equal(await page.evaluate(id => bw.store.get().mixer.nodes.find(n => n.id === id).volume, nativeId), 50);
  await participant.evaluate(() => bw.takeControl());
  await participant.waitForFunction(() => bw.store.get().role === 'controller');
  await page.waitForFunction(() => bw.store.get().role === 'participant');
  assert(await row.getByRole('slider').isDisabled());
  await raw(page, { op: 'mute', id: nativeId, value: true });
  await page.waitForFunction(() => bw.store.get().mixerError.includes('controlling'));
  await participant.getByRole('group', { name: /MixerBrowserTest/ }).getByRole('slider').fill('25');
  for (const target of [page, participant, observer]) {
    await target.waitForFunction(id => Math.abs(bw.store.get().mixer.nodes.find(n => n.id === id).volume - 25) < .1, nativeId);
  }
  for (const command of [
    { op: 'volume', id: nativeId, value: 101 },
    { op: 'mute', id: nativeId, value: true, server: 'arbitrary' },
    { op: 'mute', id: 'stale:1', value: true },
  ]) {
    await participant.evaluate(() => bw.store.set({ mixerError: '' }));
    await raw(participant, command);
    await participant.waitForFunction(() => bw.store.get().mixerError.length > 0);
  }
  assert.equal(await participant.evaluate(id => bw.store.get().mixer.nodes.find(n => n.id === id).mute, nativeId), false);
  console.log('read-only/participant rejection, handoff, shared authoritative state and malformed/stale errors passed');
  await participant.evaluate(() => bw.spawn('gst-launch-1.0 -q audiotestsrc is-live=true freq=880 volume=0.2 ! audioconvert ! pulsesink sync=false stream-properties=properties,application.name=BrowserTest'));
  await page.waitForFunction(() => bw.store.get().mixer.nodes.filter(n => n.kind === 'playback' && n.application === 'BrowserTest').length === 2);
  let clientEnv;
  for (const id of (await readdir('/proc')).filter(n => /^\d+$/.test(n))) {
    try {
      const env = Object.fromEntries((await readFile('/proc/' + id + '/environ', 'utf8')).split('\0').filter(s => s.includes('=')).map(s => [s.slice(0, s.indexOf('=')), s.slice(s.indexOf('=') + 1)]));
      if (env.HOME === root + '/home' && env.PIPEWIRE_REMOTE?.includes('bw-audio-')) { clientEnv = env; break; }
    } catch {}
  }
  assert(clientEnv);
  const graph = () => JSON.parse(execFileSync('pw-dump', { env: clientEnv, timeout: 2000 }));
  const meters = () => graph().filter(o => o.info?.props?.['node.name'] === 'browser-wayland-meter');
  await waitFor(() => meters().length === 4);
  const beforeLevels = await page.evaluate(() => window.mixerLevelCount);
  await page.waitForTimeout(1100);
  const levelCount = await page.evaluate(() => window.mixerLevelCount) - beforeLevels;
  assert(levelCount >= 8 && levelCount <= 13, 'meter updates stay around 10 Hz: ' + levelCount);
  console.log('multiple native/Pulse streams in one application have separate rows and shared monitors');
  for (const target of [page, participant]) await target.getByRole('button', { name: 'Close mixer', exact: true }).click();
  assert.equal(meters().length, 4, 'read-only subscriber retains shared meters');
  await observer.getByRole('button', { name: 'Close mixer', exact: true }).click();
  await waitFor(() => meters().length === 0);
  for (const target of [page, participant, observer]) assert.equal(await target.evaluate(() => bw.store.get().mic), false);
  console.log('last subscriber removes all monitors without starting browser capture');
  await participant.close();
  await page.waitForFunction(() => bw.store.get().role === 'controller');
  await page.getByRole('button', { name: 'Session audio mixer', exact: true }).click();
  await waitFor(() => meters().length === 4);
  await page.getByRole('button', { name: 'Fullscreen (browser shortcuts go to the desktop)', exact: true }).click();
  await page.waitForFunction(() => !!document.fullscreenElement);
  await waitFor(() => meters().length === 0);
  await page.evaluate(() => document.exitFullscreen());
  await waitFor(() => meters().length === 4);
  await page.evaluate(() => { Object.defineProperty(document, 'hidden', { configurable: true, value: true }); document.dispatchEvent(new Event('visibilitychange')); });
  await waitFor(() => meters().length === 0);
  await page.evaluate(() => { delete document.hidden; document.dispatchEvent(new Event('visibilitychange')); });
  await waitFor(() => meters().length === 4);
  console.log('hidden page and fullscreen suspend mixer monitoring; visible panel restores it');
  await page.clock.install();
  await page.clock.pauseAt(new Date());
  await page.evaluate(() => { window.holdMixerVolume = true; });
  await row.getByRole('slider').fill('70');
  await page.clock.runFor(100);
  assert(await page.evaluate(() => !!window.heldMixerVolume));
  await page.evaluate(() => bw.store.set({ mixerError: '' }));
  await raw(page, { op: 'mute', id: 'stale:1', value: true });
  await waitFor(() => page.evaluate(() => bw.store.get().mixerError.length > 0));
  assert.equal(await row.getByRole('slider').inputValue(), '70', 'unrelated error preserves pending slider value');
  await page.evaluate(() => { window.holdMixerVolume = false; originalSocketSend.call(checkSocket, window.heldMixerVolume); });
  await waitFor(() => page.evaluate(id => Math.abs(bw.store.get().mixer.nodes.find(n => n.id === id).volume - 70) < .1, nativeId));
  await page.clock.resume();
  await page.evaluate(() => Object.defineProperty(checkSocket, 'bufferedAmount', { configurable: true, get: () => 300000 }));
  await row.getByRole('slider').fill('65');
  await page.waitForFunction(() => bw.store.get().mixerError.includes('not sent'));
  assert.equal(await row.getByRole('slider').inputValue(), '70', 'socket backpressure immediately rolls back the unsent draft');
  await page.evaluate(() => { delete checkSocket.bufferedAmount; });
  console.log('controller departure hands off; unrelated errors preserve pending slider changes');
  const generation = await page.evaluate(() => bw.store.get().mixer.generation);
  const manager = graph().find(o => o.type === 'PipeWire:Interface:Client' && o.info.props['application.id'] === 'browser-wayland-mixer');
  assert(manager);
  execFileSync('pw-cli', ['destroy', String(manager.id)], { env: clientEnv, timeout: 2000 });
  await page.waitForFunction(generation => bw.store.get().mixer.available && bw.store.get().mixer.generation !== generation && bw.store.get().mixer.nodes.some(n => n.name === 'MixerBrowserTest'), generation);
  assert.equal(await page.evaluate(id => bw.store.get().mixer.nodes.some(n => n.id === id), nativeId), false);
  await page.evaluate(() => bw.store.set({ mixerError: '' }));
  await raw(page, { op: 'mute', id: nativeId, value: true });
  await page.waitForFunction(() => bw.store.get().mixerError.includes('earlier connection'));
  const currentId = await page.evaluate(() => bw.store.get().mixer.nodes.find(n => n.name === 'MixerBrowserTest').id);
  await page.waitForFunction(id => bw.store.get().mixerLevels[id] > .03, currentId);
  execFileSync('pw-cli', ['create-node', 'adapter', '{ factory.name = support.null-audio-sink node.name = OtherBrowser node.description = OtherBrowser media.class = Audio/Sink node.virtual = true node.always-process = true monitor.channel-volumes = true audio.position = [ FL FR ] object.linger = true }'], { env: clientEnv, timeout: 2000 });
  await page.waitForFunction(() => bw.store.get().mixer.nodes.some(n => n.name === 'OtherBrowser'));
  const otherId = await page.evaluate(() => bw.store.get().mixer.nodes.find(n => n.name === 'OtherBrowser').id);
  await row.getByRole('combobox').selectOption(otherId);
  await page.waitForFunction(([id, target]) => bw.store.get().mixer.nodes.find(n => n.id === id).targets.includes(target) && Math.abs(bw.store.get().mixerLevels[target] - .0343) < .002, [currentId, otherId]);
  await row.getByRole('combobox').selectOption('');
  await page.waitForFunction(id => !bw.store.get().mixer.nodes.find(n => n.name === 'MixerBrowserTest').targets.includes(id) && bw.store.get().mixerLevels[id] === 0, otherId);
  await panel.getByRole('group', { name: 'OtherBrowser Output', exact: true }).getByRole('button', { name: 'Make default', exact: true }).click();
  await page.waitForFunction(id => bw.store.get().mixer.nodes.find(n => n.id === id).is_default, otherId);
  console.log('graph reconnect refreshes UI IDs; target/default widgets route real signal');
  const rotated = await fetch('http://127.0.0.1:8088/api/token/rotate', { method: 'POST', headers: { Authorization: 'Bearer ' + token } });
  assert.equal(rotated.status, 200);
  await page.waitForFunction(() => bw.store.get().status !== 'connected' && bw.store.get().mixer.nodes.length === 0);
  await observer.waitForFunction(() => bw.store.get().status !== 'connected' && bw.store.get().mixer.nodes.length === 0);
  const rejected = new WebSocket('ws://127.0.0.1:8088/ws');
  rejected.binaryType = 'arraybuffer';
  const leaked = []; let closed = 0;
  rejected.onmessage = ({ data }) => { if ([0x0f, 0x10].includes(new Uint8Array(data)[0])) leaked.push(data); };
  rejected.onclose = event => { closed = event.code; };
  await waitFor(() => rejected.readyState === WebSocket.OPEN);
  const auth = new TextEncoder().encode(token), packet = new Uint8Array(auth.length + 1);
  packet[0] = 0x80; packet.set(auth, 1); rejected.send(packet);
  rejected.send(new Uint8Array([0x81, 0, 16, 5, 0]));
  await waitFor(() => closed);
  assert.equal(closed, 4001); assert.equal(leaked.length, 0);
  console.log('token rotation clears mixer UI; revoked credentials receive no mixer snapshot or levels');



  }

} catch (error) {
  await writeFile('/tmp/bw-private-audio-failure.log', await readFile(root + '/desktop.log'));
  throw error;
} finally {
  await browser?.close();
  desktop.kill('SIGTERM');
  await new Promise(resolve => { if (desktop.exitCode !== null) resolve(); else desktop.once('exit', resolve); });
  await log.close();
  await rm(root, { recursive: true, force: true });
}
