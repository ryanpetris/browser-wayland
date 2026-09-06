// Run inside the Docker rig with the mounted release binary.
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, readdir, rm, open, writeFile } from 'node:fs/promises';
import { spawn, execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { chromium } from 'playwright-core';

const root = await mkdtemp(tmpdir() + '/bw-private-check-');
await mkdir(root + '/home'); await mkdir(root + '/runtime', { mode: 0o700 });
const log = await open(root + '/desktop.log', 'w');
const desktop = spawn('/src/target/release/browser-wayland', ['--no-tls', '--no-rtc', '--render-node', 'none', '--listen', '127.0.0.1:8088', '--socket-name', 'wayland-private-check'], {
  env: { ...process.env, HOME: root + '/home', XDG_RUNTIME_DIR: root + '/runtime', XDG_CONFIG_HOME: root + '/config', PULSE_SINK: 'inherited-wrong-sink', PULSE_SOURCE: 'inherited-wrong-source', PIPEWIRE_NODE: '99999' },
  stdio: ['ignore', log.fd, log.fd],
});
let browser;
const recorders = [];
const wav = Buffer.alloc(44 + 48000 * 2);
wav.write('RIFF'); wav.writeUInt32LE(wav.length - 8, 4); wav.write('WAVEfmt ', 8);
wav.writeUInt32LE(16, 16); wav.writeUInt16LE(1, 20); wav.writeUInt16LE(1, 22);
wav.writeUInt32LE(48000, 24); wav.writeUInt32LE(96000, 28);
wav.writeUInt16LE(2, 32); wav.writeUInt16LE(16, 34); wav.write('data', 36);
wav.writeUInt32LE(wav.length - 44, 40);
for (let n = 0; n < 48000; n++) wav.writeInt16LE(Math.round(6000 * Math.sin(2 * Math.PI * 440 * n / 48000)), 44 + n * 2);
await writeFile(root + '/microphone.wav', wav);

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
  browser = await chromium.launch({ executablePath: '/usr/bin/chromium', env: { ...process.env, HOME: root + '/home', XDG_CONFIG_HOME: root + '/browser-config', XDG_RUNTIME_DIR: root + '/runtime' }, args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required', '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream', '--use-file-for-fake-audio-capture=' + root + '/microphone.wav'] });
  const page = await browser.newPage();
  await page.addInitScript(() => {
    const Original = window.WebSocket;
    window.WebSocket = class extends Original {
      constructor(...args) { super(...args); window.checkSocket = this; }
    };
  });
  await page.goto('http://127.0.0.1:8088/#token=' + token);
  await page.waitForFunction(() => window.bw?.store.get().status === 'connected');
  assert.equal(await page.evaluate(() => bw.store.get().audioAvailable), true);
  await page.evaluate(() => {
    for (const length of [0, 65_537]) {
      const packet = new Uint8Array(length + 1);
      packet[0] = 0x93;
      window.checkSocket.send(packet);
    }
  });
  await page.waitForTimeout(500);
  assert.equal(await page.evaluate(() => bw.store.get().audioAvailable), true);
  console.log('invalid microphone lengths leave audio available');
  await page.evaluate(() => bw.spawn('gst-launch-1.0 -q audiotestsrc is-live=true num-buffers=300 freq=440 volume=0.1 ! audioconvert ! audio/x-raw,rate=48000,channels=2 ! pipewiresink sync=false'));
  await page.waitForTimeout(2000);
  console.log('native audio stats', await page.evaluate(() => bw.store.get().stats.audio));
  let clientEnv;
  for (const id of (await readdir('/proc')).filter(n => /^\d+$/.test(n))) {
    try {
      const env = Object.fromEntries((await readFile('/proc/' + id + '/environ', 'utf8')).split('\0').filter(s => s.includes('=')).map(s => [s.slice(0, s.indexOf('=')), s.slice(s.indexOf('=') + 1)]));
      if (env.HOME === root + '/home' && env.PIPEWIRE_REMOTE?.includes('bw-audio-')) {
        clientEnv = env;
        assert.equal(env.PULSE_SINK, undefined);
        assert.equal(env.PULSE_SOURCE, undefined);
        assert.equal(env.PIPEWIRE_NODE, undefined);
        break;
      }
    } catch {}
  }
  await page.waitForFunction(() => bw.store.get().stats.audio?.signalPeak > .05);
  console.log('native playback through private default passed');
  await page.evaluate(() => bw.spawn('gst-launch-1.0 -q audiotestsrc is-live=true num-buffers=300 freq=880 volume=0.1 ! audioconvert ! pulsesink'));
  await page.evaluate(() => { bw.store.get().playback.source.smoothingTimeConstant = 0; });
  await page.waitForTimeout(1000);
  const tones = await page.evaluate(() => {
    const { context, source } = bw.store.get().playback;
    const bins = new Float32Array(source.frequencyBinCount);
    source.getFloatFrequencyData(bins);
    return [440, 880].map(hz => {
      const at = Math.round(hz * source.fftSize / context.sampleRate);
      return Math.max(...bins.slice(at - 1, at + 2));
    });
  });
  console.log('mixed tone levels', tones);
  assert(tones.every(db => db > -40), 'native and Pulse tones are both in the session mix');
  console.log('Pulse playback through private default passed');
  assert(clientEnv, 'shared launch exported private selectors');
  for (const [command, args] of [
    ['pw-record', ['--raw', '--format=f32', '--rate=48000', '--channels=1', '-']],
    ['parec', ['--raw', '--format=float32le', '--rate=48000', '--channels=1', '--latency-msec=20']],
  ]) {
    const child = spawn(command, args, { env: clientEnv, stdio: ['ignore', 'pipe', 'pipe'] });
    const samples = [];
    child.stdout.on('data', data => {
      let peak = 0;
      for (let n = 0; n + 4 <= data.length; n += 4) peak = Math.max(peak, Math.abs(data.readFloatLE(n)));
      samples.push({ at: Date.now(), peak, frames: data.length / 4 });
    });
    recorders.push({ command, child, samples });
  }
  await page.waitForTimeout(1000);
  console.log('idle recording', recorders.map(({ command, samples }) => ({ command, buffers: samples.length, peak: Math.max(0, ...samples.map(s => s.peak)) })));
  const graph = JSON.parse(execFileSync('pw-dump', { env: clientEnv, timeout: 2000 }));
  const streams = graph.filter(o => o.type.endsWith('Node') && o.info.props['media.class']?.startsWith('Stream/') && !o.info.props['node.name']?.startsWith('browser-wayland'));
  assert(streams.filter(o => o.info.props['media.class'] === 'Stream/Output/Audio').length >= 2);
  assert(streams.filter(o => o.info.props['media.class'] === 'Stream/Input/Audio').length >= 2);
  assert(streams.every(o => o.info.params.Props?.some(p => Array.isArray(p.channelVolumes))), 'application volume controls are exposed in the native graph');
  for (const { command, samples } of recorders) assert(samples.length > 0 && samples.every(s => s.peak < .0001), command + ' idle silence');
  await page.context().grantPermissions(['microphone']);
  await page.evaluate(() => bw.mic.start());
  await page.waitForFunction(() => bw.store.get().mic);
  await page.waitForTimeout(2000);
  for (const { command, samples } of recorders) assert(samples.some(s => s.peak > .02), command + ' receives browser microphone');
  await page.evaluate(() => bw.mic.stop());
  const stopped = Date.now();
  await page.waitForTimeout(1500);
  for (const { command, samples } of recorders) {
    const off = samples.filter(s => s.at > stopped + 700);
    assert(off.length > 0 && off.every(s => s.peak < .0001), command + ' produces silence after microphone stops');
  }
  console.log('native and Pulse recording: idle silence, browser microphone, and stopped silence passed');
  await page.evaluate(() => bw.mic.start());
  await page.waitForFunction(() => bw.store.get().mic);
  const children = (await readdir('/proc')).filter(n => /^\d+$/.test(n));
  let daemon;
  for (const id of children) {
    try {
      const status = await readFile('/proc/' + id + '/status', 'utf8');
      const cmd = (await readFile('/proc/' + id + '/cmdline', 'utf8')).split('\0');
      if (status.includes('PPid:\t' + desktop.pid + '\n') && cmd[0] === 'pipewire') daemon = Number(id);
    } catch {}
  }
  assert(daemon, 'owned PipeWire process found');
  process.kill(daemon, 'SIGKILL');
  await page.waitForFunction(() => !bw.store.get().audioAvailable && !bw.store.get().micAvailable && !bw.store.get().mic && !bw.store.get().playback);
  assert.equal(await page.evaluate(() => bw.store.get().status), 'connected');
  console.log('service failure withdraws live playback and microphone while desktop stays connected');
} catch (error) {
  await writeFile('/tmp/bw-private-audio-failure.log', await readFile(root + '/desktop.log'));
  throw error;
} finally {
  for (const { child } of recorders) child.kill('SIGTERM');
  await browser?.close();
  desktop.kill('SIGTERM');
  await new Promise(resolve => { if (desktop.exitCode !== null) resolve(); else desktop.once('exit', resolve); });
  await log.close();
  await rm(root, { recursive: true, force: true });
}
