// Run in the Docker image with Chromium and Node installed, after npm run build.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile, readdir } from 'node:fs/promises';
import { chromium } from 'playwright-core';

const dist = new URL('../dist/', import.meta.url);
const files = await readdir(dist, { recursive: true });
const server = createServer(async (req, res) => {
  try {
    const path = new URL(req.url, 'http://localhost').pathname;
    const data = await readFile(new URL(path === '/' ? 'index.html' : path.slice(1), dist));
    res.setHeader('Content-Type', path.endsWith('.js') ? 'text/javascript' : path.endsWith('.css') ? 'text/css' : 'text/html');
    res.end(data);
  } catch { res.writeHead(404).end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const browser = await chromium.launch({ executablePath: process.env.CHROMIUM || '/usr/bin/chromium', args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required'] });
try {
  const context = await browser.newContext({ deviceScaleFactor: 2 });
  const page = await context.newPage();
  const errors = [], chunks = [];
  page.on('pageerror', e => errors.push(e.message));
  page.on('request', r => { if (/visualiser-.*\.js/.test(r.url())) chunks.push(r.url()); });
  await page.addInitScript(() => {
    window.graphEdges = [];
    const connect = AudioNode.prototype.connect, disconnect = AudioNode.prototype.disconnect;
    AudioNode.prototype.connect = function (node, ...rest) {
      window.graphEdges.push([this, node]); return connect.call(this, node, ...rest);
    };
    AudioNode.prototype.disconnect = function (node, ...rest) {
      window.graphEdges = window.graphEdges.filter(([from, to]) => from !== this || (node && to !== node));
      return node ? disconnect.call(this, node, ...rest) : disconnect.call(this);
    };
    window.framesDrawn = 0;
    const fill = CanvasRenderingContext2D.prototype.fillRect;
    CanvasRenderingContext2D.prototype.fillRect = function (...args) { window.framesDrawn++; return fill.apply(this, args); };
  });
  let releaseChunk, requestedChunk;
  const chunkGate = new Promise(resolve => { releaseChunk = resolve; });
  const chunkRequested = new Promise(resolve => { requestedChunk = resolve; });
  await page.route('**/assets/visualiser-*.js', async route => {
    requestedChunk(); await chunkGate; await route.continue();
  });
  await page.goto(process.env.ELSEWHERE_TEST_URL || `http://127.0.0.1:${server.address().port}`);
  await page.waitForFunction(() => !!window.elsewhere?.store);
  await page.evaluate(() => window.elsewhere.store.set({ status: 'connected', role: 'viewer', audioAvailable: true, micAvailable: false }));
  assert.equal(chunks.length, 0, 'renderer must not load until opened');
  const aboutButton = page.getByRole('button', { name: 'About / Licenses & source', exact: true });
  await aboutButton.click();
  const about = page.getByRole('dialog', { name: 'About Elsewhere' });
  await about.waitFor();
  assert.equal(await about.locator('a').count(), 2, 'About offers licenses and the source repository');
  assert(!files.some(f => /viewer-source|audiomotion-source|audiomotion-LICENSE/.test(f)), 'source downloads are not embedded');
  for (const link of await about.locator('a[href^="/"]').all()) {
    const response = await context.request.get(new URL(await link.getAttribute('href'), page.url()).href);
    assert.equal(response.status(), 200, 'About download is served');
  }
  await about.focus();
  await page.keyboard.press('Shift+Tab');
  assert(await about.locator('a').last().evaluate(node => node === document.activeElement), 'About keeps keyboard focus inside');
  assert.equal(await about.locator('a').last().evaluate(node => {
    let escaped = false;
    const listener = () => { escaped = true; };
    document.addEventListener('paste', listener);
    node.dispatchEvent(new ClipboardEvent('paste', { bubbles: true }));
    document.removeEventListener('paste', listener);
    return escaped;
  }), false, 'About paste stays out of the remote clipboard');
  await page.keyboard.press('Escape');
  assert(await aboutButton.evaluate(node => node === document.activeElement), 'Escape restores About trigger focus');
  assert.equal(chunks.length, 0, 'About does not load the visualiser');

  await page.getByRole('button', { name: 'Session audio mixer', exact: true }).click();
  await page.getByRole('region', { name: 'Session audio mixer', exact: true }).waitFor();
  await page.getByRole('button', { name: 'Close mixer', exact: true }).click();
  assert.equal(chunks.length, 0, 'mixer does not load the optional visualiser');

  await page.getByRole('button', { name: 'Audio visualiser', exact: true }).click();
  await page.getByText('Waiting for session audio.', { exact: false }).waitFor();
  await page.evaluate(async () => {
    const context = new AudioContext({ sampleRate: 48000 });
    await context.resume();
    const source = context.createAnalyser();
    source.connect(context.destination);
    const oscillator = context.createOscillator();
    oscillator.connect(source); oscillator.start();
    window.testPlayback = { context, source, oscillator };
    window.testAudioTick = setInterval(() => {
      const { context, source } = window.testPlayback;
      const samples = new Float32Array(source.fftSize);
      source.getFloatTimeDomainData(samples);
      const signalPeak = Math.max(...samples.map(Math.abs));
      elsewhere.store.set({ stats: { ...elsewhere.store.get().stats, audio: { packets: 1, decoded: 1, lead: 0, state: context.state, signalPeak, level: signalPeak > .0001 ? 200 : 0 } } });
    }, 50);
    window.elsewhere.store.set({ playback: { context, source }, stats: { ...window.elsewhere.store.get().stats, audio: { packets: 1, decoded: 1, lead: 0, state: 'running', signalPeak: .1, level: 200 } } });
  });
  const panel = page.getByRole('region', { name: 'Session audio' });
  await chunkRequested;
  await panel.getByRole('button', { name: 'Close visualiser', exact: true }).click();
  releaseChunk();
  await page.waitForTimeout(200);
  assert.equal(await page.locator('[aria-label="Session audio"] canvas').count(), 0, 'closed during loading stays disposed');
  await page.getByRole('button', { name: 'Audio visualiser', exact: true }).click();
  await panel.locator('canvas').waitFor({ timeout: 5000 }).catch(async e => { console.log(await page.locator('body').innerText(), errors); throw e; });
  assert.equal(chunks.length, 1);
  const checkEdges = async expected => {
    assert.equal(await page.evaluate(() => window.graphEdges.filter(([from]) => from === window.testPlayback.source).length), expected);
    assert.equal(await page.evaluate(() => window.graphEdges.filter(([from, to]) => from === window.testPlayback.source && to === window.testPlayback.context.destination).length), 1);
  };
  await checkEdges(2);
  await panel.getByText('Session signal received.', { exact: false }).waitFor();
  await page.evaluate(() => window.testPlayback.context.suspend());
  await panel.getByText('Playback is waiting for a user gesture.', { exact: false }).waitFor();
  await page.evaluate(() => window.testPlayback.context.resume());
  await panel.getByText('Session signal received.', { exact: false }).waitFor();
  for (const style of ['line', 'radial', 'stereo', 'bars']) {
    await panel.getByRole('combobox', { name: /^Style/ }).selectOption(style);
    await checkEdges(2);
  }
  assert(await panel.locator('canvas').evaluate(c => c.width >= c.clientWidth * 1.9), 'HiDPI canvas');
  await panel.getByRole('button', { name: 'Fullscreen visualiser', exact: true }).click();
  await page.waitForFunction(() => !!document.fullscreenElement);
  assert(await panel.isVisible(), 'fullscreen panel remains visible');
  await checkEdges(2);
  await panel.getByRole('button', { name: 'Exit fullscreen', exact: true }).click();
  await page.waitForFunction(() => !document.fullscreenElement);
  await page.getByRole('button', { name: 'Fullscreen (browser shortcuts go to the desktop)', exact: true }).click();
  await page.waitForFunction(() => !!document.fullscreenElement);
  await page.waitForTimeout(100);
  await checkEdges(1);
  assert.equal(await panel.isVisible(), false);
  await page.evaluate(() => document.exitFullscreen());
  await page.waitForTimeout(100);
  await checkEdges(2);
  await page.evaluate(() => {
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => true });
    document.dispatchEvent(new Event('visibilitychange'));
  });
  await page.waitForTimeout(100);
  await checkEdges(1);
  await page.evaluate(() => {
    delete document.hidden;
    document.dispatchEvent(new Event('visibilitychange'));
  });
  await page.waitForTimeout(100);
  await checkEdges(2);
  await panel.getByLabel('Animate', { exact: true }).uncheck();
  await checkEdges(1);
  await page.waitForTimeout(300);
  const stopped = await page.evaluate(() => window.framesDrawn);
  await page.waitForTimeout(500);
  assert.equal(await page.evaluate(() => window.framesDrawn), stopped, 'animation off stops drawing');
  await panel.getByLabel('Animate', { exact: true }).check();
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await page.waitForTimeout(100);
  await checkEdges(1);
  await page.emulateMedia({ reducedMotion: 'no-preference' });
  await page.waitForTimeout(100);
  await checkEdges(2);
  const cdp = await context.newCDPSession(page);
  const listeners = async () => {
    const { result } = await cdp.send('Runtime.evaluate', { expression: 'window' });
    return (await cdp.send('DOMDebugger.getEventListeners', { objectId: result.objectId })).listeners.filter(l => l.type === 'click').length;
  };
  const clickListeners = await listeners();
  for (let i = 0; i < 8; i++) {
    await panel.getByRole('button', { name: 'Close visualiser', exact: true }).click();
    await checkEdges(1);
    await page.getByRole('button', { name: 'Audio visualiser', exact: true }).click();
    await panel.locator('canvas').waitFor();
    await checkEdges(2);
  }
  assert.equal(await listeners(), clickListeners, 'no leaked window click listeners');
  await panel.getByRole('combobox', { name: /^Style/ }).selectOption('bars');
  await panel.getByRole('combobox', { name: /^Colours/ }).selectOption('classic');
  // Classic bars have coloured pixels only when the renderer draws a signal.
  // Check this canvas, since each open creates a fresh renderer and FFT input.
  const renderedImage = async signal => {
    const result = await page.waitForFunction(signal => {
      const canvas = document.querySelector('[aria-label="Session audio"] canvas');
      if (!canvas?.width || !canvas.height) return false;
      const pixels = canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height).data;
      let coloured = 0;
      for (let i = 0; i < pixels.length; i += 4) {
        if (pixels[i + 3] && Math.max(pixels[i], pixels[i + 1], pixels[i + 2]) - Math.min(pixels[i], pixels[i + 1], pixels[i + 2]) > 30) coloured++;
      }
      return (signal ? coloured > 100 : coloured === 0 && pixels[3] === 255) && canvas.toDataURL();
    }, signal, { timeout: 5000 }).catch(cause => {
      throw new Error(`Visualiser did not render ${signal ? 'coloured signal bars' : 'silence with no coloured pixels'}`, { cause });
    });
    try { return await result.jsonValue(); } finally { await result.dispose(); }
  };
  const signalImage = await renderedImage(true);
  await page.evaluate(() => {
    window.testPlayback.oscillator.stop();
    window.elsewhere.store.set({ stats: { ...window.elsewhere.store.get().stats, audio: { packets: 1, decoded: 1, lead: 0, state: 'running', signalPeak: 0, level: 0 } } });
  });
  await panel.getByText('Connected, but silent.', { exact: false }).waitFor();
  assert.notEqual(await renderedImage(false), signalImage, 'signal and silence draw differently');
  // Same wrapper must also follow a replacement playback graph.
  await page.evaluate(() => {
    window.oldPlayback = window.testPlayback;
    const context = new AudioContext(), source = context.createAnalyser();
    source.connect(context.destination);
    window.testPlayback = { context, source };
    window.elsewhere.store.set({ playback: { context, source } });
  });
  await page.waitForTimeout(200);
  await checkEdges(2);
  assert.equal(await page.evaluate(() => window.graphEdges.filter(([from]) => from === window.oldPlayback.source).length), 1);
  await panel.getByRole('button', { name: 'Close visualiser', exact: true }).click();
  await checkEdges(1);
  assert.equal(await page.evaluate(() => window.testPlayback.context.state === 'closed'), false);
  assert.equal(await panel.locator('canvas').count(), 0);
  assert.equal(errors.length, 0, errors.join('\n'));
  await page.route('**/assets/visualiser-*.js', route => route.abort());
  await page.reload();
  await page.waitForFunction(() => !!window.elsewhere?.store);
  await page.evaluate(async () => {
    const context = new AudioContext(); await context.resume();
    const source = context.createAnalyser(); source.connect(context.destination);
    window.testPlayback = { context, source };
    elsewhere.store.set({ status: 'connected', role: 'viewer', audioAvailable: true, micAvailable: false, playback: { context, source } });
  });
  await page.getByRole('button', { name: 'Audio visualiser', exact: true }).click();
  await page.getByRole('alert').filter({ hasText: 'Visualiser unavailable' }).waitFor();
  await checkEdges(1);
  assert.equal(await page.evaluate(() => window.testPlayback.context.state), 'running');
  assert.equal(await page.getByRole('region', { name: 'Session audio' }).locator('canvas').count(), 0);
  assert.equal(errors.length, 0, errors.join('\n'));
  console.log('renderer failure isolation, lazy load, styles, HiDPI, fullscreen, animation off, reduced motion, hidden page/stage, close while loading, eight open/close cycles, listener and graph ownership, silence, source replacement passed');
} finally { await browser.close(); server.close(); }
