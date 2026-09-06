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
    window.WebSocket = class extends Original {
      constructor(...args) { super(...args); window.checkSocket = this; }
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
  console.log('multiple native/Pulse streams in one application have separate rows and shared monitors');
  for (const target of [page, participant]) await target.getByRole('button', { name: 'Close mixer', exact: true }).click();
  assert.equal(meters().length, 4, 'read-only subscriber retains shared meters');
  await observer.getByRole('button', { name: 'Close mixer', exact: true }).click();
  await waitFor(() => meters().length === 0);
  for (const target of [page, participant, observer]) assert.equal(await target.evaluate(() => bw.store.get().mic), false);
  console.log('last subscriber removes all monitors without starting browser capture');
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
