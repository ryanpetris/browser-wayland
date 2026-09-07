// Run in Docker. Firefox uses geckodriver on port 4445; Chromium uses Playwright.
import assert from 'node:assert/strict';
import { readFile, mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { createServer } from 'node:http';
import { chromium } from 'playwright-core';

const worklet = await readFile(new URL('../src/mic-worklet.js', import.meta.url), 'utf8');
const source = await readFile(new URL('../src/mic.js', import.meta.url), 'utf8');
const mic = source.replace("import workletSource from './mic-worklet.js?raw';", `const workletSource = ${JSON.stringify(worklet)};`);
assert.notEqual(mic, source, 'check the microphone worklet import used by this runner');
const check = await readFile(new URL('./microphone-browser.js', import.meta.url), 'utf8');
const server = createServer((req, res) => {
  res.setHeader('Content-Type', req.url.endsWith('.js') ? 'text/javascript' : 'text/html');
  res.end(req.url === '/mic.js' ? mic : req.url === '/check.js' ? check : '<button id="start">Start</button><script type="module">import {checkMicrophone} from "/check.js"; window.checkMicrophone = checkMicrophone; document.querySelector("button").onclick = () => checkMicrophone().then(result => window.result = result, e => window.result = {error: e.stack ?? e.message});</script>');
});
await new Promise(r => server.listen(8099, '127.0.0.1', r));
let browser, session;
const root = await mkdtemp(tmpdir() + '/elsewhere-microphone-');
const wav = Buffer.alloc(44 + 48000 * 2);
wav.write('RIFF'); wav.writeUInt32LE(wav.length - 8, 4); wav.write('WAVEfmt ', 8);
wav.writeUInt32LE(16, 16); wav.writeUInt16LE(1, 20); wav.writeUInt16LE(1, 22);
wav.writeUInt32LE(48000, 24); wav.writeUInt32LE(96000, 28);
wav.writeUInt16LE(2, 32); wav.writeUInt16LE(16, 34); wav.write('data', 36);
wav.writeUInt32LE(wav.length - 44, 40);
for (let n = 0; n < 48000; n++) wav.writeInt16LE(Math.round(6000 * Math.sin(2 * Math.PI * 440 * n / 48000)), 44 + n * 2);
await writeFile(root + '/microphone.wav', wav);
const wd = async (path, body, method = body ? 'POST' : 'GET') => {
  const response = await fetch('http://127.0.0.1:4445' + path, { method, headers: { 'Content-Type': 'application/json' }, body: body ? JSON.stringify(body) : undefined });
  const data = await response.json();
  if (!response.ok) throw new Error(JSON.stringify(data));
  return data.value;
};
try {
  if (process.argv.includes('--firefox')) {
    session = (await wd('/session', { capabilities: { alwaysMatch: { browserName: 'firefox', 'moz:firefoxOptions': { binary: '/usr/bin/firefox', args: ['-headless'], prefs: { 'media.navigator.streams.fake': true, 'media.navigator.permission.disabled': true } } } } })).sessionId;
    const route = '/session/' + session;
    await wd(route + '/timeouts', { script: 60000, pageLoad: 20000 });
    await wd(route + '/url', { url: 'http://127.0.0.1:8099' });
    const button = await wd(route + '/element', { using: 'css selector', value: '#start' });
    await wd(route + '/element/' + Object.values(button)[0] + '/click', {});
    const result = await wd(route + '/execute/async', { script: 'const done=arguments[0]; const timer=setTimeout(()=>done({error:"microphone check timed out: " + JSON.stringify(window.micCheckState?.())}),30000); const poll=()=>window.result ? (clearTimeout(timer),done(window.result)) : setTimeout(poll,50); poll();', args: [] });
    assert(!result.error, result.error);
    console.log('Firefox', result);
  } else {
    browser = await chromium.launch({ executablePath: '/usr/bin/chromium', args: ['--no-sandbox', '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream', '--use-file-for-fake-audio-capture=' + root + '/microphone.wav'] });
    const page = await browser.newPage();
    await page.goto('http://127.0.0.1:8099');
    await page.waitForFunction(() => window.checkMicrophone);
    await page.locator('#start').click();
    await page.waitForFunction(() => window.result, null, { timeout: 30000 });
    const result = await page.evaluate(() => window.result);
    assert(!result.error, result.error);
    console.log('Chromium', result);
  }
} finally {
  await browser?.close();
  if (session) await wd('/session/' + session, undefined, 'DELETE');
  await new Promise(r => server.close(r));
  await rm(root, { recursive: true, force: true });
}
