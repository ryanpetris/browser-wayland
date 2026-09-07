// Docker rig: Firefox, foot, Xvfb and geckodriver on port 4445 with a headed display.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, mkdir, open, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';

const root = await mkdtemp(tmpdir() + '/elsewhere-paste-');
await mkdir(root + '/runtime', { mode: 0o700 });
const log = await open(root + '/server.log', 'w');
const origin = 'http://127.0.0.1:8096';
const server = spawn(process.env.ELSEWHERE_BINARY || '/src/target/release/elsewhere', ['--no-audio', '--no-rtc', '--no-tls', '--render-node', 'none', '--codec', 'vp8', '--listen', '127.0.0.1:8096', '--socket-name', 'wayland-paste'], {
  cwd: root, env: { ...process.env, HOME: root, XDG_CONFIG_HOME: root + '/config', XDG_RUNTIME_DIR: root + '/runtime' }, stdio: ['ignore', log.fd, log.fd],
});
const contents = path => readFile(path, 'utf8').catch(() => null);
const wait = async predicate => {
  for (let i = 0; i < 200; i++) { if (await predicate()) return; await new Promise(r => setTimeout(r, 50)); }
  throw new Error('clipboard condition timed out');
};
const wd = async (path, body, method = body ? 'POST' : 'GET') => {
  const response = await fetch('http://127.0.0.1:4445' + path, { method, headers: { 'Content-Type': 'application/json' }, body: body ? JSON.stringify(body) : undefined });
  const data = await response.json();
  if (!response.ok) throw new Error(JSON.stringify(data.value));
  return data.value;
};
let route;
try {
  await wait(async () => { try { return (await fetch(origin)).ok && !!await contents(root + '/config/elsewhere/token'); } catch { return false; } });
  const token = (await contents(root + '/config/elsewhere/token')).trim();
  const viewerToken = (await contents(root + '/config/elsewhere/viewer-token')).trim();
  const session = await wd('/session', { capabilities: { alwaysMatch: { browserName: 'firefox', 'moz:firefoxOptions': { binary: '/usr/bin/firefox' } } } });
  route = '/session/' + session.sessionId;
  console.log(session.capabilities.browserName, session.capabilities.browserVersion);
  const js = (script, ...args) => wd(route + '/execute/sync', { script, args });
  const click = async selector => {
    const el = await wd(route + '/element', { using: 'css selector', value: selector });
    await wd(route + '/element/' + Object.values(el)[0] + '/click', {});
  };
  const keys = async values => wd(route + '/actions', { actions: [{ type: 'key', id: 'keyboard', actions: [
    ...values.map(value => ({ type: 'keyDown', value })), ...values.toReversed().map(value => ({ type: 'keyUp', value })),
  ] }] });
  let navigation = 0;
  const load = async (token, id) => {
    await wd(route + '/url', { url: `${origin}/?check=${++navigation}${id ? '&window=' + id : ''}#token=${token}` });
    await wait(() => js('return !!window.elsewhere?.store.get().stream'));
  };
  await load(token);
  await js('elsewhere.spawn(arguments[0])', `foot --app-id=paste-check sh -c 'cat > ${root}/pasted'`);
  await wait(() => js('return elsewhere.store.get().windows.some(w => w.app_id === "paste-check")'));
  await wait(async () => await contents(root + '/pasted') === '');
  const id = await js('return elsewhere.store.get().windows.find(w => w.app_id === "paste-check").id');
  let expected = '';
  for (const windowId of [null, id]) {
    await load(token, windowId);
    const main = await wd(route + '/window');
    for (const pip of [false, true]) {
      if (pip) {
        const before = await wd(route + '/window/handles');
        await click('button[title="Picture-in-picture"]');
        await wait(async () => (await wd(route + '/window/handles')).length > before.length);
        const handle = (await wd(route + '/window/handles')).find(handle => !before.includes(handle));
        await wd(route + '/window', { handle });
        await wd(route + '/frame', { id: 0 });
        await wait(() => js('return !!window.elsewhere?.store.get().stream'));
      }
      await js('elsewhere.activate(arguments[0]); document.addEventListener("paste", e => { window.lastPaste = { trusted: e.isTrusted, text: e.clipboardData.getData("text/plain") }; });', id);
      await click('canvas');
      const text = `${windowId ? 'window' : 'desktop'}${pip ? ' pip' : ''} clipboard`;
      await js('return navigator.clipboard.writeText(arguments[0])', text);
      await keys(['\uE009', '\uE008', 'v']);
      await wait(() => js('return window.lastPaste?.trusted && lastPaste.text === arguments[0]', text));
      await wait(() => js('return elsewhere.clipboard.read().then(text => text === arguments[0])', text));
      await keys(['\uE007']);
      expected += text + '\n';
      await wait(async () => await contents(root + '/pasted') === expected);
      assert.equal(await js('return document.querySelector("canvas").hasAttribute("contenteditable")'), false);
      assert.equal(await js('return document.querySelector("canvas").childNodes.length'), 0);
      // A local editable field must retain normal paste and leave the remote clipboard alone.
      await js('const field = document.createElement("textarea"); field.id = "local-paste"; field.style="position:fixed;top:0;left:0;z-index:9999"; document.body.append(field)');
      await click('#local-paste');
      await js('return navigator.clipboard.writeText("local field")');
      await keys(['\uE009', '\uE008', 'v']);
      await wait(() => js('return document.querySelector("#local-paste").value === "local field"'));
      assert.equal(await js('return elsewhere.clipboard.read()'), text);
      await js('document.querySelector("#local-paste").remove()');
      console.log(text, 'trusted paste, application round trip, canvas cleanup and local field isolation');
      if (pip) {
        await js('window.parent.elsewhereReturn()');
        await wd(route + '/window', { handle: main });
      }
    }
    await load(viewerToken, windowId);
    assert.equal(await js('return elsewhere.store.get().role'), 'viewer');
    await click('canvas');
    await js('return navigator.clipboard.writeText("forbidden")');
    await keys(['\uE009', '\uE008', 'v']);
    assert.equal(await js('return document.querySelector("canvas").hasAttribute("contenteditable")'), false);
    assert.notEqual(await js('return elsewhere.clipboard.read()'), 'forbidden');
  }
} catch (error) {
  console.error(error, await contents(root + '/server.log'));
  throw error;
} finally {
  if (route) await wd(route, undefined, 'DELETE');
  server.kill('SIGTERM');
  await new Promise(resolve => { if (server.exitCode !== null || server.signalCode !== null) resolve(); else server.once('exit', resolve); });
  await log.close();
  await rm(root, { recursive: true, force: true, maxRetries: 5 });
}
