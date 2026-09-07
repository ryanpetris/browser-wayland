// Run inside the Docker rig with the mounted release binary.
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, readdir, rm, open, writeFile } from 'node:fs/promises';
import { spawn, execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { chromium } from 'playwright-core';

const rtc = process.argv.includes('--rtc');
const root = await mkdtemp(tmpdir() + '/elsewhere-private-check-');
await mkdir(root + '/home'); await mkdir(root + '/runtime', { mode: 0o700 });
const log = await open(root + '/desktop.log', 'w');
const desktop = spawn('/src/target/release/elsewhere', ['--no-tls', ...(rtc ? [] : ['--no-rtc']), '--render-node', 'none', '--listen', '127.0.0.1:8088', '--socket-name', 'wayland-private-check'], {
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
      await readFile(root + '/config/elsewhere/token');
      return (await fetch('http://127.0.0.1:8088/')).ok;
    } catch { return false; }
  });
  const token = (await readFile(root + '/config/elsewhere/token', 'utf8')).trim();
  browser = await chromium.launch({ executablePath: '/usr/bin/chromium', env: { ...process.env, HOME: root + '/home', XDG_CONFIG_HOME: root + '/browser-config', XDG_RUNTIME_DIR: root + '/runtime' }, args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required', '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream', '--use-file-for-fake-audio-capture=' + root + '/microphone.wav'] });
  const page = await browser.newPage();
  await page.addInitScript(() => {
    window.micTracks = [];
    window.micWorklets = [];
    const getUserMedia = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
    window.realMicCapture = async options => {
      const stream = await getUserMedia(options);
      micTracks.push(...stream.getTracks());
      return stream;
    };
    navigator.mediaDevices.getUserMedia = realMicCapture;
    const Worklet = window.AudioWorkletNode;
    window.AudioWorkletNode = class extends Worklet {
      constructor(...args) { super(...args); micWorklets.push(args[1]); }
    };
    window.audioSocketPackets = 0;
    const Peer = window.RTCPeerConnection;
    window.RTCPeerConnection = class extends Peer {
      createDataChannel(...args) { const ch = super.createDataChannel(...args); window.audioCheckChannel = ch; return ch; }
    };
    const Original = window.WebSocket;
    window.WebSocket = class extends Original {
      constructor(...args) { super(...args); window.checkSocket = this; this.addEventListener('message', ({ data }) => { if (data instanceof ArrayBuffer && new Uint8Array(data)[0] === 5) audioSocketPackets++; }); }
    };
  });
  await page.goto('http://127.0.0.1:8088/#token=' + token);
  await page.waitForFunction(() => window.elsewhere?.store.get().status === 'connected');
  assert.equal(await page.evaluate(() => elsewhere.store.get().audioAvailable), true);
  if (!rtc) {
    await page.evaluate(() => elsewhere.setTransport('webrtc'));
    assert.deepEqual(await page.evaluate(() => [elsewhere.store.get().rtcAvailable, elsewhere.store.get().rtcRecovery.state, localStorage.getItem('elsewhere.transport')]), [false, 'unavailable', 'webrtc']);
    await page.waitForTimeout(1200);
    assert.equal(await page.evaluate(() => window.audioCheckChannel), undefined);
    await page.evaluate(() => elsewhere.setTransport('websocket'));
    console.log('disabled server preserves WebRTC preference without attempts');
  }

  await page.evaluate(() => {
    for (const length of [0, 65_537]) {
      const packet = new Uint8Array(length + 1);
      packet[0] = 0x93;
      window.checkSocket.send(packet);
    }
  });
  await page.waitForTimeout(500);
  assert.equal(await page.evaluate(() => elsewhere.store.get().audioAvailable), true);
  console.log('invalid microphone lengths leave audio available');
  await page.evaluate(() => elsewhere.spawn('gst-launch-1.0 -q audiotestsrc is-live=true num-buffers=900 freq=440 volume=0.1 ! audioconvert ! audio/x-raw,rate=48000,channels=2 ! pipewiresink sync=false'));
  await page.waitForTimeout(2000);
  console.log('native audio stats', await page.evaluate(() => elsewhere.store.get().stats.audio));
  let clientEnv;
  for (const id of (await readdir('/proc')).filter(n => /^\d+$/.test(n))) {
    try {
      const env = Object.fromEntries((await readFile('/proc/' + id + '/environ', 'utf8')).split('\0').filter(s => s.includes('=')).map(s => [s.slice(0, s.indexOf('=')), s.slice(s.indexOf('=') + 1)]));
      if (env.HOME === root + '/home' && env.PIPEWIRE_REMOTE?.includes('elsewhere-audio-')) {
        clientEnv = env;
        assert.equal(env.PULSE_SINK, undefined);
        assert.equal(env.PULSE_SOURCE, undefined);
        assert.equal(env.PIPEWIRE_NODE, undefined);
        break;
      }
    } catch {}
  }
  await page.waitForFunction(() => elsewhere.store.get().stats.audio?.signalPeak > .05);
  console.log('native playback through private default passed');
  if (rtc) {
    await page.evaluate(() => elsewhere.setTransport('webrtc'));
    await page.waitForFunction(() => elsewhere.store.get().videoVia === 'webrtc');
    const before = await page.evaluate(() => audioSocketPackets);
    await page.waitForFunction(n => audioSocketPackets > n + 10, before);
    await page.evaluate(() => audioCheckChannel.close());
    await page.waitForFunction(() => elsewhere.store.get().rtcRecovery.state === 'waiting');
    const fallback = await page.evaluate(() => audioSocketPackets);
    await page.waitForFunction(n => audioSocketPackets > n + 5, fallback);
    await page.waitForFunction(() => elsewhere.store.get().videoVia === 'webrtc');
    assert.equal(await page.evaluate(() => elsewhere.store.get().transport), 'webrtc');
    assert(await page.evaluate(() => elsewhere.store.get().stats.audio.signalPeak > .05));
    console.log('real session audio stayed on WebSocket during RTC playback, fallback and recovery');
  }

  await page.evaluate(() => elsewhere.spawn('gst-launch-1.0 -q audiotestsrc is-live=true num-buffers=300 freq=880 volume=0.1 ! audioconvert ! pulsesink'));
  await page.evaluate(() => { elsewhere.store.get().playback.source.smoothingTimeConstant = 0; });
  await page.waitForTimeout(1000);
  const tones = await page.evaluate(() => {
    const { context, source } = elsewhere.store.get().playback;
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
  const streams = graph.filter(o => o.type.endsWith('Node') && o.info.props['media.class']?.startsWith('Stream/') && !o.info.props['node.name']?.startsWith('elsewhere'));
  assert(streams.filter(o => o.info.props['media.class'] === 'Stream/Output/Audio').length >= 2);
  assert(streams.filter(o => o.info.props['media.class'] === 'Stream/Input/Audio').length >= 2);
  assert(streams.every(o => o.info.params.Props?.some(p => Array.isArray(p.channelVolumes))), 'application volume controls are exposed in the native graph');
  for (const { command, samples } of recorders) assert(samples.length > 0 && samples.every(s => s.peak < .0001), command + ' idle silence');
  await page.context().grantPermissions(['microphone']);
  await page.evaluate(() => elsewhere.mic.start());
  await page.waitForFunction(() => elsewhere.store.get().mic);
  await page.waitForTimeout(2000);
  for (const { command, samples } of recorders) assert(samples.some(s => s.peak > .02), command + ' receives browser microphone');
  await page.waitForFunction(() => elsewhere.store.get().mixer.nodes.some(n => n.kind === 'input' && n.mute_writable));
  await page.evaluate(() => {
    const input = elsewhere.store.get().mixer.nodes.find(n => n.kind === 'input');
    elsewhere.mixer.command({ op: 'mute', id: input.id, value: true });
  });
  await page.waitForFunction(() => elsewhere.store.get().mixer.nodes.find(n => n.kind === 'input').mute);
  const muted = Date.now();
  await page.waitForTimeout(1200);
  assert.equal(await page.evaluate(() => elsewhere.store.get().mic), true, 'session mute preserves browser capture');
  for (const { command, samples } of recorders) {
    const quiet = samples.filter(s => s.at > muted + 600);
    assert(quiet.length > 0 && quiet.every(s => s.peak < .0001), command + ' observes session microphone mute');
  }
  await page.evaluate(() => {
    const input = elsewhere.store.get().mixer.nodes.find(n => n.kind === 'input');
    elsewhere.mixer.command({ op: 'mute', id: input.id, value: false });
  });
  await page.waitForFunction(() => !elsewhere.store.get().mixer.nodes.find(n => n.kind === 'input').mute);
  const resumed = Date.now();
  await page.evaluate(() => {
    const json = new TextEncoder().encode(JSON.stringify({ op: 'subscribe', enabled: true }));
    const packet = new Uint8Array(json.length + 1); packet[0] = 0x97; packet.set(json, 1);
    window.mixerFlood = setInterval(() => { for (let n = 0; n < 64; n++) checkSocket.send(packet); }, 10);
  });
  await page.waitForTimeout(1200);
  await page.evaluate(() => { clearInterval(window.mixerFlood); elsewhere.mixer.subscribe(false); });
  console.log('microphone under subscription traffic', recorders.map(({ command, samples }) => ({ command, buffers: samples.filter(s => s.at > resumed + 600).length, peak: Math.max(0, ...samples.filter(s => s.at > resumed + 600).map(s => s.peak)) })));
  for (const { command, samples } of recorders) {
    const active = samples.filter(s => s.at > resumed + 600);
    // Capture processing attenuates a steady tone; progress needs samples and signal above muted silence.
    assert(active.reduce((n, s) => n + s.frames, 0) >= 9600 && active.some(s => s.peak > .005), command + ' microphone progresses under subscription traffic');
  }
  console.log('session microphone mute preserves consent; microphone resumes under sustained mixer traffic');
  await page.evaluate(() => elsewhere.mic.stop());
  const stopped = Date.now();
  await page.waitForTimeout(1500);
  for (const { command, samples } of recorders) {
    const off = samples.filter(s => s.at > stopped + 700);
    assert(off.length > 0 && off.every(s => s.peak < .0001), command + ' produces silence after microphone stops');
  }
  console.log('native and Pulse recording: idle silence, browser microphone, and stopped silence passed');
  for (let cycle = 0; cycle < 3; cycle++) {
    const started = Date.now();
    await page.evaluate(() => elsewhere.mic.start());
    await waitFor(() => recorders.every(({ samples }) => samples.some(s => s.at > started && s.peak > .005)));
    await page.evaluate(() => elsewhere.mic.stop());
    assert(await page.evaluate(() => micTracks.every(t => t.readyState === 'ended')), 'each capture stop ends all tracks');
    const stopped = Date.now();
    await page.waitForTimeout(1200);
    for (const { command, samples } of recorders) {
      const quiet = samples.filter(s => s.at > stopped + 700);
      assert(quiet.length > 0 && quiet.every(s => s.peak < .0001), command + ' is silent between capture cycles');
    }
  }
  assert(await page.evaluate(() => micWorklets.filter(name => name === 'microphone-capture').length >= 4), 'real delivery uses AudioWorklet');
  // The actual button cancels a pending permission request; old success/denial cannot revive it.
  for (const denied of [false, true]) {
    await page.evaluate(() => {
      navigator.mediaDevices.getUserMedia = () => new Promise((resolve, reject) => { window.resolveMic = resolve; window.rejectMic = reject; });
    });
    await page.getByRole('button', { name: 'Microphone', exact: true }).click();
    await page.locator('button[aria-label="Microphone"][aria-pressed="true"]').waitFor();
    await page.getByRole('button', { name: 'Microphone', exact: true }).click();
    await page.waitForFunction(() => !elsewhere.store.get().mic);
    await page.evaluate(async denied => {
      navigator.mediaDevices.getUserMedia = realMicCapture;
      await elsewhere.mic.start();
      if (denied) rejectMic(new DOMException('Denied', 'NotAllowedError'));
      else resolveMic(await realMicCapture({ audio: true }));
    }, denied);
    await page.waitForTimeout(200);
    assert(await page.evaluate(() => elsewhere.store.get().mic), 'old permission result preserves new capture');
    await page.evaluate(() => elsewhere.mic.stop());
    assert(await page.evaluate(() => micTracks.every(t => t.readyState === 'ended')), 'stopping after a stale permission result ends all tracks');
  }
  await page.evaluate(async () => {
    await elsewhere.mic.start();
    micTracks.find(t => t.readyState === 'live').dispatchEvent(new Event('ended'));
  });
  await page.waitForFunction(() => !elsewhere.store.get().mic && elsewhere.store.get().notice?.text === 'microphone: Microphone disconnected');
  assert(await page.evaluate(() => micTracks.every(t => t.readyState === 'ended')), 'capture failure stops tracks and reports the reason');
  const participant = await browser.newPage();
  await participant.goto('http://127.0.0.1:8088/#token=' + token);
  await participant.waitForFunction(() => window.elsewhere?.store.get().role === 'participant');
  await page.evaluate(() => elsewhere.mic.start());
  await participant.evaluate(() => elsewhere.takeControl());
  await page.waitForFunction(() => elsewhere.store.get().role === 'participant' && !elsewhere.store.get().mic);
  assert(await page.evaluate(() => micTracks.every(t => t.readyState === 'ended')), 'handover stops tracks');
  await page.evaluate(() => elsewhere.takeControl());
  await page.waitForFunction(() => elsewhere.store.get().role === 'controller');
  await participant.close();
  await page.evaluate(async () => { await elsewhere.mic.start(); checkSocket.close(); });
  await page.waitForFunction(() => !elsewhere.store.get().mic && micTracks.every(t => t.readyState === 'ended'));
  await page.waitForFunction(() => elsewhere.store.get().status === 'connected' && elsewhere.store.get().role === 'controller');
  console.log('AudioWorklet repeated delivery, pending permission cancellation, handover and disconnect passed');
  await page.evaluate(() => elsewhere.mic.start());
  await page.waitForFunction(() => elsewhere.store.get().mic);
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
  await page.waitForFunction(() => !elsewhere.store.get().audioAvailable && !elsewhere.store.get().micAvailable && !elsewhere.store.get().mic && !elsewhere.store.get().playback);
  assert.equal(await page.evaluate(() => elsewhere.store.get().status), 'connected');
  console.log('service failure withdraws live playback and microphone while desktop stays connected');
} catch (error) {
  await writeFile('/tmp/elsewhere-private-audio-failure.log', await readFile(root + '/desktop.log'));
  throw error;
} finally {
  for (const { child } of recorders) child.kill('SIGTERM');
  await browser?.close();
  desktop.kill('SIGTERM');
  await new Promise(resolve => { if (desktop.exitCode !== null) resolve(); else desktop.once('exit', resolve); });
  await log.close();
  await rm(root, { recursive: true, force: true });
}
