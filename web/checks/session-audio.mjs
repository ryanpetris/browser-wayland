// Docker live-session check. ELSEWHERE_TEST_TOKEN_FILE points at the rig's control token.
import { spawnSync } from 'node:child_process';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { chromium } from 'playwright-core';
const cleanup = () => {
  spawnSync('pkill', ['-f', '^gst-launch-1.0 -q (audio|video)testsrc']);
  spawnSync('pkill', ['-f', '^mpv --no-config --no-audio --length=40 --title=elsewhere-audio-check']);
};
cleanup();
const token = (await readFile(process.env.ELSEWHERE_TEST_TOKEN_FILE, 'utf8')).trim();
const browser = await chromium.launch({ executablePath: '/usr/bin/chromium', args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required', '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream'] });
try {
  const page = await browser.newPage();
  await page.goto(`${process.env.ELSEWHERE_TEST_URL || 'http://127.0.0.1:8080'}/#token=${token}`);
  await page.waitForFunction(() => window.elsewhere?.store.get().status === 'connected');
  await page.evaluate(() => {
    elsewhere.setChoice({ codec: 'vp8', quality: 'low' });
    elsewhere.spawn('gst-launch-1.0 -q audiotestsrc is-live=true num-buffers=1600 freq=440 volume=0.1 ! audioconvert ! pulsesink');
    elsewhere.spawn('mpv --no-config --no-audio --length=40 --title=elsewhere-audio-check --vo=wlshm av://lavfi:testsrc2=size=640x360:rate=30');
  });
  await page.waitForFunction(() => elsewhere.store.get().stats.audio?.level > 0 && elsewhere.store.get().stats.frames > 5, { timeout: 15000 });
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('Performance.enable');
  const metrics = async () => Object.fromEntries((await cdp.send('Performance.getMetrics')).metrics.map(m => [m.name, m.value]));
  const measure = async () => {
    const before = await metrics();
    const start = await page.evaluate(() => elsewhere.store.get().stats.frames);
    const peak = await page.evaluate(async () => {
      let peak = 0;
      for (let n = 0; n < 20; n++) {
        const source = elsewhere.store.get().playback.source, samples = new Float32Array(source.fftSize);
        source.getFloatTimeDomainData(samples);
        for (const sample of samples) peak = Math.max(peak, Math.abs(sample));
        await new Promise(resolve => setTimeout(resolve, 150));
      }
      return peak;
    });
    const after = await metrics();
    return { peak, taskMsPerSecond: 1000 * (after.TaskDuration - before.TaskDuration) / 3, ...(await page.evaluate(start => ({ frames: elsewhere.store.get().stats.frames - start, audio: elsewhere.store.get().stats.audio, underruns: elsewhere.store.get().stats.underruns }), start)) };
  };
  const closed = await measure();
  await page.getByRole('button', { name: 'Audio visualiser', exact: true }).click();
  const panel = page.getByRole('region', { name: 'Session audio' });
  await panel.locator('canvas').waitFor();
  const image1 = await panel.locator('canvas').evaluate(c => c.toDataURL());
  await page.waitForTimeout(300);
  const image2 = await panel.locator('canvas').evaluate(c => c.toDataURL());
  assert.notEqual(image1, image2, 'actual playback animates spectrum');
  const open = await measure();
  console.log(JSON.stringify({ closed, open }));
  assert(open.frames > 30 && open.audio.level > 0);
  assert(Math.abs(open.peak - closed.peak) < .002, 'analysis does not alter playback level');
  await panel.getByRole('button', { name: 'Close visualiser', exact: true }).click();
  const closedAgain = await measure();
  console.log(JSON.stringify({ closed, open, closedAgain }));
  // Stop the rig's finite test signal early, then observe the real playback analyser.
  await page.evaluate(() => elsewhere.spawn("pkill -f '^gst-launch-1.0 -q audiotestsrc'"));
  await page.waitForFunction(() => elsewhere.store.get().stats.audio?.signalPeak < 0.0001);
  await page.getByRole('button', { name: 'Audio visualiser', exact: true }).click();
  await panel.getByText('Connected, but silent.', { exact: false }).waitFor();
  console.log('actual decoded session playback, animated spectrum, stable volume, video decoding and silence passed');
  assert.equal(await page.evaluate(() => elsewhere.store.get().mic), false, 'visualisation does not start capture');
  await page.context().grantPermissions(['microphone']);
  await page.evaluate(() => elsewhere.mic.start());
  await page.waitForFunction(() => elsewhere.store.get().mic);
  await page.evaluate(() => elsewhere.mic.stop());
  await page.waitForFunction(() => !elsewhere.store.get().mic);
  console.log('fake-device microphone start/stop passed');
} finally { await browser.close(); cleanup(); }
