// Run in the Docker rig after building the release binary.
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, open, readFile, rm } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { tmpdir } from 'node:os';
import { chromium } from 'playwright-core';

const root = await mkdtemp(tmpdir() + '/elsewhere-sizing-');
await mkdir(root + '/runtime', { mode: 0o700 });
const log = await open(root + '/server.log', 'w');
const origin = 'http://127.0.0.1:8093';
const server = spawn('/src/target/release/elsewhere', ['--no-audio', '--no-rtc', '--no-tls', '--render-node', 'none', '--codec', 'vp8', '--listen', '127.0.0.1:8093', '--socket-name', 'wayland-sizing'], {
  env: { ...process.env, HOME: root, XDG_CONFIG_HOME: root + '/config', XDG_RUNTIME_DIR: root + '/runtime', RUST_LOG: 'elsewhere_server::api=debug' }, stdio: ['ignore', log.fd, log.fd],
});
const wait = async fn => {
  for (let i = 0; i < 200; i++) { if (await fn()) return; await new Promise(r => setTimeout(r, 100)); }
  throw new Error('timed out');
};
const dimensions = png => [png.readUInt32BE(16), png.readUInt32BE(20)];
let browser;
try {
  await wait(async () => { try { return (await fetch(origin)).ok && !!await readFile(root + '/config/elsewhere/token'); } catch { return false; } });
  const token = (await readFile(root + '/config/elsewhere/token', 'utf8')).trim();
  const viewerToken = (await readFile(root + '/config/elsewhere/viewer-token', 'utf8')).trim();
  const headers = { Authorization: `Bearer ${viewerToken}` };
  assert.equal((await fetch(origin + '/api/screenshot.png')).status, 401);
  let session, rpcId = 0;
  const rpc = async (method, params) => {
    const response = await fetch(origin + '/mcp', { method: 'POST', headers: { ...headers, 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream', ...(session ? { 'Mcp-Session-Id': session } : {}) }, body: JSON.stringify({ jsonrpc: '2.0', id: ++rpcId, method, params }) });
    session ||= response.headers.get('Mcp-Session-Id');
    const body = await response.text();
    if (!response.ok) return { error: { status: response.status, message: body } };
    const data = body.split('\n').filter(l => l.startsWith('data:')).map(l => l.slice(5).trim()).filter(Boolean);
    assert(body.trim(), `empty MCP response ${response.status}`);
    const result = data.length ? JSON.parse(data.at(-1)) : JSON.parse(body);
    return result;
  };
  await rpc('initialize', { protocolVersion: '2025-03-26', capabilities: {}, clientInfo: { name: 'sizing-check', version: '1' } });
  browser = await chromium.launch({ env: { ...process.env, XDG_CONFIG_HOME: root + '/chromium' }, executablePath: '/usr/bin/chromium', args: ['--no-sandbox'] });
  for (const [dpr, viewport] of [[1, { width: 1000, height: 700 }], [1.5, { width: 700, height: 1000 }], [2, { width: 1000, height: 700 }]]) {
    const context = await browser.newContext({ viewport, deviceScaleFactor: dpr });
    await context.addInitScript(() => localStorage.setItem('elsewhere.sidebar', '1'));
    const page = await context.newPage();
    const previews = [];
    page.on('request', request => { if (request.url().includes('/snapshot.png')) previews.push(new URL(request.url()).searchParams); });
    await page.goto(origin + '/#token=' + token);
    await page.waitForFunction(() => !!elsewhere.store.get().stream);
    await page.evaluate(() => elsewhere.takeControl());
    for (const [name, size] of [['landscape', '640x360'], ['portrait', '300x600']]) {
      await page.evaluate(({ name, size }) => elsewhere.spawn(`foot --app-id=sizing-${name} --window-size-pixels=${size}`), { name, size });
      await page.waitForFunction(name => elsewhere.store.get().windows.some(w => w.app_id === `sizing-${name}`), name);
    }
    await page.waitForTimeout(750);
    const windows = await (await fetch(origin + '/api/windows', { headers })).json();
    await wait(() => previews.some(q => q.get('width') === String(Math.ceil(64 * dpr))));
    assert(previews.some(q => q.get('width') === String(Math.ceil(64 * dpr))), 'landscape list previews use width');
    await wait(() => previews.some(q => q.get('height') === String(Math.ceil(40 * dpr))));
    assert(previews.some(q => q.get('height') === String(Math.ceil(40 * dpr))), 'portrait list previews use height');
    const stream = await page.evaluate(() => elsewhere.store.get().stream);
    for (const id of [null, ...windows.filter(w => w.app_id.startsWith('sizing-')).map(w => w.id)]) {
      const path = id == null ? '/api/screenshot.png' : `/api/windows/${id}/snapshot.png`;
      const target = windows.find(w => w.id === id);
      const native = target ? [target.w * stream.scale, target.h * stream.scale] : [stream.width, stream.height];
      for (const sizing of [{}, { width: 64 }, { height: 40 }, { width: 320 }, { percentage: 50 }, { percentage: 0.001 }]) {
        const ratio = sizing.width ? sizing.width / native[0] : sizing.height ? sizing.height / native[1] : sizing.percentage ? sizing.percentage / 100 : 1;
        const expected = native.map(n => Math.max(1, Math.round(n * ratio)));
        const response = await fetch(origin + path + '?' + new URLSearchParams(sizing), { headers });
        assert.equal(response.status, 200, await response.clone().text().then(t => t.slice(0, 100)));
        const png = Buffer.from(await response.arrayBuffer());
        assert.deepEqual(dimensions(png), expected, JSON.stringify({ id, sizing, native, stream }));
        const result = await rpc('tools/call', { name: id == null ? 'screenshot' : 'snapshot', arguments: { ...(id == null ? {} : { window: id }), ...sizing } });
        assert(!result.error && !result.result.isError, JSON.stringify(result));
        assert.deepEqual(dimensions(Buffer.from(result.result.content[0].data, 'base64')), expected);
        console.log(JSON.stringify({ dpr, target: target?.app_id ?? 'desktop', sizing, dimensions: dimensions(png), pngBytes: png.length }));
      }
      for (const query of ['width=1&height=2', 'width=1&width=2', 'scale=.5', 'percentage=50&scale=.5', 'width=0', 'height=-1', 'width=1.5', 'percentage=NaN', 'percentage=Infinity', 'percentage=201', 'width=16385', 'height=16384', 'percentage=bad', 'width=', 'percentage=5e-324', 'widht=64', 'width=10000']) {
        const response = await fetch(origin + path + '?' + query, { headers });
        assert.equal(response.status, 400, query);
      }
      for (const fields of ['"width":32,"width":64', '"percentage":null,"percentage":50']) {
        const response = await fetch(origin + '/mcp', { method: 'POST', headers: { ...headers, 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream', 'Mcp-Session-Id': session }, body: `{"jsonrpc":"2.0","id":${++rpcId},"method":"tools/call","params":{"name":"${id == null ? 'screenshot' : 'snapshot'}","arguments":{${id == null ? '' : '"window":' + id + ','}${fields}}}}` });
        assert.equal(response.status, 400, fields);
        const repeatedEnvelope = await fetch(origin + '/mcp', { method: 'POST', headers: { ...headers, 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream', 'Mcp-Session-Id': session }, body: `{"jsonrpc":"2.0","id":${++rpcId},"method":"tools/call","method":"tools/call","params":{"name":"screenshot","arguments":{${fields}}}}` });
        assert.equal(repeatedEnvelope.status, 400, 'repeated envelope fields cannot bypass sizing validation');
      }
      for (const sizing of [{ widht: 64 }, { width: 10000 }, { width: 0 }, { width: 1, percentage: 50 }, { height: -1 }, { percentage: 201 }, { scale: 0.5 }]) {
        const result = await rpc('tools/call', { name: id == null ? 'screenshot' : 'snapshot', arguments: { ...(id == null ? {} : { window: id }), ...sizing } });
        assert(result.error || result.result.isError, JSON.stringify(sizing));
        assert(!result.error?.status, 'invalid tool arguments return a JSON-RPC error');
      }
    }
    const tiny = windows.find(w => w.app_id === 'sizing-portrait');
    previews.length = 0;
    await page.evaluate(tiny => elsewhere.store.set({ windows: elsewhere.store.get().windows.map(w => w.id === tiny.id ? { ...w, w: 3, h: 5 } : w) }), tiny);
    await wait(() => previews.some(q => q.get('height') === String(Math.max(1, Math.floor(5 * stream.scale)))));
    for (const win of windows.filter(w => w.app_id.startsWith('sizing-'))) await page.evaluate(id => elsewhere.control({ id, op: 'close' }), win.id);
    await context.close();
  }
  console.log((await readFile(root + '/server.log', 'utf8')).split('\n').filter(l => l.includes('snapshot completed')).join('\n'));
} finally {
  await browser?.close(); server.kill('SIGTERM');
  await new Promise(resolve => server.exitCode != null ? resolve() : server.once('exit', resolve));
  await log.close(); await rm(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
}
