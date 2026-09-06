// Docker rig: release build and Chromium. Exercises HTTP permissions,
// filesystem races, and two viewers.
import assert from 'node:assert/strict';
import {execFileSync, spawn} from 'node:child_process';
import {
  chmod,
  mkdir,
  mkdtemp,
  open,
  readdir,
  readFile,
  rename,
  rm,
  stat,
  symlink,
  writeFile
} from 'node:fs/promises';
import {chromium} from 'playwright-core';

const root = await mkdtemp('/tmp/bw-files-');
await mkdir(root + '/runtime', {mode : 0o700});
for (const name of ['a', 'b', 'large', 'blocked', 'gone'])
  await mkdir(root + '/' + name);
await writeFile(root + '/a/hello ü.txt', 'hello');
await writeFile(root + '/a/.hidden', 'hidden');
await writeFile(root + '/a/fstatfs-fallback.txt', 'fallback works');
await writeFile(
    Buffer.concat([ Buffer.from(root + '/a/'), Buffer.from([ 255 ]) ]),
    'non-UTF8');
await symlink(root + '/a/hello ü.txt', root + '/a/link');
await symlink(root + '/b', root + '/a/dir-link');
await symlink(root + '/absent', root + '/a/broken');
execFileSync('mkfifo', [ root + '/a/pipe' ]);
await symlink(root + '/a/pipe', root + '/a/pipe-link');
await Promise.all(Array.from(
    {length : 603},
    (_, i) => writeFile(root + '/large/' + String(i).padStart(4, '0'), 'x')));
await chmod(root + '/blocked', 0);
const log = await open(root + '/server.log', 'w');
const origin = 'http://127.0.0.1:8094';
const server =
    spawn('/src/target/release/browser-wayland',
          [
            '--no-rtc', '--no-tls', '--render-node', 'none', '--codec', 'vp8',
            '--listen', '127.0.0.1:8094', '--socket-name', 'wayland-files-check'
          ],
          {
            env : {
              ...process.env,
              HOME : root,
              XDG_CONFIG_HOME : root + '/config',
              XDG_RUNTIME_DIR : root + '/runtime'
            },
            stdio : [ 'ignore', log.fd, log.fd ]
          });
const wait = async fn => {
  for (let i = 0; i < 200; i++) {
    if (await fn())
      return;
    await new Promise(r => setTimeout(r, 100));
  }
  throw Error('timed out');
};
let browser, inputClient, inputLog;
try {
  await wait(async () => {
    try {
      return !!await readFile(root + '/config/browser-wayland/token');
    } catch {
      return false;
    }
  });
  const token =
      (await readFile(root + '/config/browser-wayland/token', 'utf8')).trim();
  const viewerToken =
      (await readFile(root + '/config/browser-wayland/viewer-token', 'utf8'))
          .trim();
  const request = (url, options = {}, key = token) => fetch(origin + url, {
    ...options,
    headers : {...options.headers, Authorization : `Bearer ${key}`}
  });
  const location = (name, path) =>
      `/api/files/${encodeURIComponent(name)}?${new URLSearchParams({path})}`;
  const list = async path => {
    const r = await request('/api/files?' + new URLSearchParams({path}));
    assert.equal(r.status, 200);
    return r.json();
  };
  for (const [url, method] of [[ '/api/files?path=@transfer', 'GET' ],
                               [ location('hello ü.txt', root + '/a'), 'GET' ],
                               [ location('x', root + '/a'), 'PUT' ],
                               [ location('x', root + '/a'), 'DELETE' ],
                               [ '/api/clipboard/files/0', 'GET' ]])
    assert.equal((await request(url, {method}, viewerToken)).status, 403);
  assert.equal(
      (await request('/api/files', {
        method : 'POST',
        headers : {'Content-Type' : 'application/json'},
        body : JSON.stringify({op : 'mkdir', path : root, name : 'denied'})
      },
                     viewerToken))
          .status,
      403);
  const rpcSessions = new Map();
  let rpcId = 0;
  const rpc = async (method, params, key = viewerToken) => {
    const rpcSession = rpcSessions.get(key);
    const r = await request('/mcp', {
      method : 'POST',
      headers : {
        'Content-Type' : 'application/json',
        Accept : 'application/json, text/event-stream',
        ...(rpcSession ? {'Mcp-Session-Id' : rpcSession} : {})
      },
      body : JSON.stringify({jsonrpc : '2.0', id : ++rpcId, method, params})
    },
                            key);
    if (r.headers.has('Mcp-Session-Id')) rpcSessions.set(key, r.headers.get('Mcp-Session-Id'));
    const text = await r.text();
    return JSON.parse(text.split('\n')
                          .filter(line => line.startsWith('data:'))
                          .map(line => line.slice(5).trim())
                          .filter(Boolean)
                          .at(-1) ||
                      text);
  };
  await rpc('initialize', {
    protocolVersion : '2025-03-26',
    capabilities : {},
    clientInfo : {name : 'files-check', version : '1'}
  });
  assert.equal((await rpc('tools/call', {name : 'files', arguments : {path : '@transfer'}}))
                   .result.isError,
               true);
  assert.equal((await list('@transfer')).path, root + '/Downloads');
  assert.equal((await list('@home')).path, root);
  for (const [url, method] of [['/api/files', 'GET'], ['/api/files/x', 'GET'], ['/api/files/x', 'PUT'], ['/api/files/x', 'DELETE']]) {
    assert.equal((await request(url, {method})).status, 400, `${method} requires path`);
  }
  await rpc('initialize', {protocolVersion: '2025-03-26', capabilities: {}, clientInfo: {name: 'files-control-check', version: '1'}}, token);
  const tools = (await rpc('tools/list', {}, token)).result.tools;
  assert(tools.find(t => t.name === 'files').inputSchema.required.includes('path'));
  const missingPath = await rpc('tools/call', {name: 'files', arguments: {}}, token);
  assert(missingPath.error || missingPath.result?.isError, JSON.stringify(missingPath));
  assert.match(JSON.stringify(missingPath), /path/);
  for (const query of [{path: '@transfer'}, {path: '@home'}, {path: root + '/a', hidden: true, sort: 'size', desc: true, offset: 1, limit: 2}]) {
    const result = await rpc('tools/call', {name: 'files', arguments: query}, token);
    assert(!result.error && !result.result.isError, JSON.stringify(result));
    const expected = await (await request('/api/files?' + new URLSearchParams(query))).json();
    assert.deepEqual(JSON.parse(result.result.content[0].text), expected);
  }
  assert((await rpc('tools/call', {name: 'files', arguments: {path: root + '/blocked'}}, token)).result.isError);

  const fallback = await request(location('fstatfs-fallback.txt', root + '/a'));
  assert.equal(fallback.status, 200);
  assert.equal(await fallback.text(), 'fallback works');
  for (const [directory, name] of [[ '/proc', 'cpuinfo' ],
                                   [ '/sys/kernel', 'uevent_seqnum' ]]) {
    const response = await request(location(name, directory));
    assert.equal(response.status, 200);
    assert.equal(response.headers.get('content-length'), null);
    assert((await response.text()).length > 0);
  }

  let listing = await list(root + '/a');
  assert.equal(listing.entries[0].name, 'dir-link');
  assert.equal(listing.omitted, 1);
  for (const sort of ['name', 'size', 'modified']) {
    const sorted = await request('/api/files?' +
                                 new URLSearchParams(
                                     {path : root + '/a', sort, desc : true}))
                       .then(r => r.json());
    assert.equal(sorted.entries[0].name, 'dir-link');
    const field = sort === 'modified' ? 'modified_ms' : sort;
    const values = sorted.entries.slice(1).map(e => e[field]);
    assert(values.every((v, i) => !i || values[i - 1] >= v));
  }

  assert(!listing.entries.some(e => e.name === '.hidden' || e.name === 'pipe'));
  assert.equal(
      (await request('/api/files?' +
                     new URLSearchParams({path : root + '/a', hidden : true}))
           .then(r => r.json()))
          .entries.some(e => e.name === '.hidden'),
      true);
  assert.equal(
      (await request(location('link', root + '/a')).then(r => r.text())),
      'hello');
  for (const name of ['pipe', 'pipe-link'])
    assert.equal((await request(location(name, root + '/a'), {
                   signal : AbortSignal.timeout(2000)
                 })).status,
                 422);
  assert.equal((await list(root + '/a/dir-link')).path, root + '/b');
  for (const [path, status] of [[ root + '/blocked', 403 ],
                                [ root + '/absent', 404 ], [ 'relative', 400 ]])
    assert.equal(
        (await request('/api/files?' + new URLSearchParams({path}))).status,
        status);
  assert.equal((await list(root + '/large')).entries.length, 100);
  assert.equal((await list(root + '/large')).total, 603);
  const collision = await Promise.all(
      Array.from({length : 8}, () => request(location('same.txt', root + '/b'),
                                             {method : 'PUT', body : 'data'})
                                         .then(r => r.json())));
  assert.equal(new Set(collision.map(f => f.name)).size, 8);
  assert(collision.every(f => f.directory === root + '/b'));
  assert.equal((await stat(collision[0].path)).mode & 0o777,
               0o666 & ~process.umask());
  const action = body => request('/api/files', {
    method : 'POST',
    headers : {'Content-Type' : 'application/json'},
    body : JSON.stringify(body)
  });
  assert.equal(
      (await action({op : 'mkdir', path : root + '/b', name : '.new'})).status,
      201);
  assert.equal((await action({
                 op : 'rename',
                 path : root + '/b',
                 name : '.new',
                 new_name : 'same.txt'
               })).status,
               409);
  assert.equal((await action({
                 op : 'rename',
                 path : root + '/b',
                 name : '.new',
                 new_name : 'folder'
               })).status,
               201);
  assert.equal((await request(location('folder', root + '/b'), {
                 method : 'DELETE'
               })).status,
               409);
  assert.equal((await request(location('broken', root + '/a'), {
                 method : 'DELETE'
               })).status,
               204);
  // Hold an upload open while its destination is renamed; publication stays
  // anchored there.
  let streamController;
  const pending = request(location('held.txt', root + '/gone'), {
    method : 'PUT',
    duplex : 'half',
    body : new ReadableStream({
      start(c) {
        streamController = c;
        c.enqueue(new TextEncoder().encode('held'));
      }
    })
  });
  await wait(
      async () =>
          (await readdir(root + '/gone')).some(n => n.endsWith('.part')));
  assert.equal((await request(
                    '/api/files?' +
                    new URLSearchParams({path : root + '/gone', hidden : true}))
                    .then(r => r.json()))
                   .entries.length,
               0);
  await rename(root + '/gone', root + '/renamed');
  await mkdir(root + '/gone');
  streamController.close();
  assert.equal((await pending.then(r => r.json())).directory,
               root + '/renamed');
  assert.deepEqual(await readdir(root + '/gone'), []);
  const controller = new AbortController();
  const interrupted = request(location('cancelled', root + '/gone'), {
                        method : 'PUT',
                        duplex : 'half',
                        signal : controller.signal,
                        body : new ReadableStream(
                            {start(c) { c.enqueue(new Uint8Array([ 1 ])); }})
                      }).catch(() => null);
  await wait(async () => (await readdir(root + '/gone')).length > 0);
  controller.abort();
  await interrupted;
  await wait(async () => (await readdir(root + '/gone')).length === 0);
  const deleted = request(location('deleted', root + '/gone'), {
    method : 'PUT',
    duplex : 'half',
    body : new ReadableStream({
      start(c) {
        streamController = c;
        c.enqueue(new Uint8Array([ 1 ]));
      }
    })
  });
  await wait(async () => (await readdir(root + '/gone')).length > 0);
  await rm(root + '/gone', {recursive : true});
  streamController.close();
  assert.equal((await deleted).status, 404);
  assert.equal((await request(location('missing', root + '/gone'),
                              {method : 'PUT', body : 'x'}))
                   .status,
               404);
  console.log(
      'HTTP authorization, pagination, symlinks/special files, collisions, actions, cancellation and destination races passed');
  browser = await chromium.launch({
    headless : true,
    executablePath : '/usr/bin/chromium',
    env : {...process.env, XDG_CONFIG_HOME : root + '/chromium'},
    args : [ '--no-sandbox' ]
  });
  const context = await browser.newContext();
  const p = await context.newPage(), q = await context.newPage(),
        ro = await context.newPage();
  for (const [page, key] of [[ p, token ], [ q, token ], [ ro, viewerToken ]]) {
    await page.goto(origin + '/#token=' + key);
    await page.waitForFunction(() => !!window.bw?.store.get().role);
  }
  assert.equal(
      await ro.getByRole('button', {name : 'Files', exact : true}).count(), 0);
  for (const page of [p, q])
    await page.getByRole('button', {name : 'Files', exact : true}).click();
  const navigate = async (page, path) => {
    await page.getByRole('textbox', {name : 'Directory path'}).fill(path);
    await page.getByRole('button', {name : 'Go', exact : true}).click();
    await page.waitForFunction(
        path => bw.store.get().filesPath === path &&
                !Array
                     .from(document.querySelectorAll(
                         '[aria-label="File browser"] [role="status"]'))
                     .some(e => e.textContent.includes('Loading directory')),
        path);
  };
  await navigate(p, root + '/a');
  await navigate(q, root + '/b');
  console.log('Viewers navigated');
  const requests = [];
  p.on('request', r => {
    if (r.method() === 'GET' && r.url().includes('/api/files?'))
      requests.push(r.url());
  });
  await p.getByRole('button', {name : 'Windows', exact : true}).click();
  await writeFile(root + '/a/external', 'external');
  await p.getByRole('button', {name : 'Files', exact : true}).click();
  await p.bringToFront();
  await p.waitForTimeout(250);
  assert.equal(requests.length, 0);
  assert.equal(await p.locator('[data-file-name="external"]').count(), 0);
  await p.getByRole('button', {name : 'Refresh', exact : true}).click();
  await p.locator('[data-file-name="external"]').waitFor();
  console.log('Refresh behavior passed');
  let release;
  const held = new Promise(resolve => release = resolve);
  let reached = false;
  await p.route('**/api/files/late*', async route => {
    reached = true;
    await held;
    await route.continue();
  });
  const batch = p.evaluate(() => bw.uploadFiles([
    new File([ 'one' ], 'late.txt'), new File([ 'two' ], 'late2.txt')
  ]));
  await wait(() => reached);
  await navigate(p, root + '/large');
  const before = requests.length;
  release();
  await batch;
  console.log('Late batch completed');
  assert.equal(requests.length, before);
  assert.equal(await p.evaluate(() => bw.store.get().filesPath),
               root + '/large');
  assert.equal(await readFile(root + '/a/late2.txt', 'utf8'), 'two');
  await q.evaluate(
      () => bw.uploadFiles([ new File([ 'other' ], 'client-b.txt') ]));
  await q.locator('[data-file-name="client-b.txt"]').waitFor();
  assert.equal(await readFile(root + '/b/client-b.txt', 'utf8'), 'other');
  assert.equal(await p.locator('[data-file-name]').count(), 100);
  await p.getByRole('button', {name : 'Next', exact : true}).click();
  await p.locator('[data-file-name="0100"]').waitFor();
  await p.evaluate(path => bw.openFiles(path), root + '/b');
  await p.locator('[data-file-name="client-b.txt"]').waitFor();
  // Delay an old listing after a later navigation has completed.
  let releaseList;
  const heldList = new Promise(resolve => releaseList = resolve);
  await p.route('**/api/files?**', async route => {
    if (new URL(route.request().url()).searchParams.get('path') ===
        root + '/a') {
      await heldList;
    }
    await route.continue().catch(() => {});
  });
  await p.getByRole('textbox', {name : 'Directory path'}).fill(root + '/a');
  await p.getByRole('button', {name : 'Go', exact : true}).click();
  await navigate(p, root + '/b');
  releaseList();
  await p.waitForTimeout(200);
  assert.equal(await p.evaluate(() => bw.store.get().filesPath), root + '/b');
  await p.evaluate(() => bw.takeControl());
  await p.waitForFunction(() => bw.store.get().role === 'controller');
  await navigate(p, root + '/Downloads');
  await p.locator('canvas.stage').evaluate(el => {
    const data = new DataTransfer();
    data.items.add(new File([ 'desktop drop' ], 'rescued.txt'));
    for (const type of ['dragenter', 'dragover', 'drop'])
      el.dispatchEvent(new DragEvent(type, {
        dataTransfer : data,
        bubbles : true,
        cancelable : true,
        clientX : 100,
        clientY : 100
      }));
  });
  await p.locator('[data-file-name="rescued.txt"]').waitFor();
  assert.equal(await readFile(root + '/Downloads/rescued.txt', 'utf8'),
               'desktop drop');
  await p.getByRole('button', {name : 'Open folder', exact : true}).waitFor();
  // A late rescue offers navigation without moving the current directory.
  await navigate(p, root + '/b');
  await p.locator('canvas.stage').evaluate(el => {
    const data = new DataTransfer();
    data.items.add(new File([ 'desktop drop' ], 'rescued.txt'));
    for (const type of ['dragenter', 'dragover', 'drop'])
      el.dispatchEvent(new DragEvent(type, {
        dataTransfer : data,
        bubbles : true,
        cancelable : true,
        clientX : 100,
        clientY : 100
      }));
  });
  await p.waitForFunction(
      () => bw.store.get().notice?.text.includes('rescued (2).txt'));
  assert.equal(await p.evaluate(() => bw.store.get().filesPath), root + '/b');
  await p.getByRole('button', {name : 'Open folder', exact : true}).click();
  await p.locator('[data-file-name="rescued (2).txt"]').waitFor();
  await p.route('**/api/drop/*/fail.txt', route => route.abort());
  await p.locator('canvas.stage').evaluate(el => {
    const data = new DataTransfer();
    data.items.add(new File([ 'partial' ], 'partial.txt'));
    data.items.add(new File([ 'fail' ], 'fail.txt'));
    for (const type of ['dragenter', 'dragover', 'drop'])
      el.dispatchEvent(new DragEvent(type, {
        dataTransfer : data,
        bubbles : true,
        cancelable : true,
        clientX : 100,
        clientY : 100
      }));
  });
  await p.locator('[data-file-name="partial.txt"]').waitFor();
  assert.equal(await readFile(root + '/Downloads/partial.txt', 'utf8'),
               'partial');
  await p.locator('canvas.stage').evaluate(el => {
    const data = new DataTransfer();
    data.items.add(new File([ 'paste contents' ], 'pasted.txt'));
    el.dispatchEvent(new ClipboardEvent(
        'paste', {clipboardData : data, bubbles : true, cancelable : true}));
  });
  await p.waitForFunction(
      async () => (await bw.clipboard.read()).includes('pasted.txt'));
  const uri = (await p.evaluate(() => bw.clipboard.read())).trim();
  assert(uri.includes('/browser-wayland/drops/'));
  assert.equal(await readFile(new URL(uri), 'utf8'), 'paste contents');
  assert.equal((await request('/api/clipboard', {}, viewerToken)).status, 403);
  assert.equal(
      (await rpc('tools/call', {name : 'clipboard_read', arguments : {}}))
          .result.isError,
      true);
  assert.equal(await ro.evaluate(() => bw.store.get().clipboardFiles.length),
               0);
  assert.equal(
      await ro.locator('button[title^="Download the copied files"]').count(),
      0);
  await p.locator('canvas.stage').evaluate(el => {
    const data = new DataTransfer();
    data.items.add(new File([ 'hidden' ], '.rejected'));
    for (const type of ['dragenter', 'dragover', 'drop'])
      el.dispatchEvent(new DragEvent(type, {
        dataTransfer : data,
        bubbles : true,
        cancelable : true,
        clientX : 100,
        clientY : 100
      }));
  });
  await p.waitForFunction(() => bw.store.get().notice?.text.startsWith(
                              '1 file could not be saved'));
  await p.evaluate(cmd => bw.spawn(cmd),
                   'stdbuf -oL wev > ' + root + '/wev.log');
  await p.waitForFunction(
      () => bw.store.get().windows.some(w => w.app_id === 'wev'));
  await p.evaluate(
      () =>
          bw.activate(bw.store.get().windows.find(w => w.app_id === 'wev').id));
  await p.locator('canvas.stage').focus();
  await p.keyboard.press('a');
  await wait(
      async () =>
          (await readFile(root + '/wev.log', 'utf8')).includes('sym: a'));
  await p.waitForFunction(() => bw.store.get().stats.frames > 0);
  await p.screenshot({path : root + '/file-browser.png'});
  console.log('Native keyboard input and video frames passed');
  const otherRequests = requests.length;
  q.once('dialog', dialog => dialog.accept('renamed.txt'));
  await q.locator('[data-file-name="client-b.txt"]')
      .getByRole('button', {name : 'Rename', exact : true})
      .click();
  await q.locator('[data-file-name="renamed.txt"]').waitFor();
  assert.equal(await readFile(root + '/b/renamed.txt', 'utf8'), 'other');
  q.once('dialog', dialog => dialog.dismiss());
  await q.locator('[data-file-name="renamed.txt"]')
      .getByRole('button', {name : 'Delete', exact : true})
      .click();
  assert.equal(await readFile(root + '/b/renamed.txt', 'utf8'), 'other');
  q.once('dialog', dialog => dialog.accept());
  await q.locator('[data-file-name="renamed.txt"]')
      .getByRole('button', {name : 'Delete', exact : true})
      .click();
  await q.locator('[data-file-name="renamed.txt"]').waitFor({
    state : 'detached'
  });
  q.once('dialog', dialog => dialog.accept('created folder'));
  await q.getByRole('button', {name : 'New folder', exact : true}).click();
  await q.locator('[data-file-name="created folder"]').waitFor();
  assert.equal(requests.length, otherRequests);
  console.log(
      'UI rename, confirmed deletion, creation and other-client isolation passed');

  console.log(
      'Native desktop unclaimed drop, collision rescue, explicit navigation and file-paste staging passed');
  console.log(
      'Two-client destinations, late batches, local mutation refresh, no reopen/focus refresh, and stale responses passed');
} finally {
  inputClient?.kill('SIGTERM');
  await inputLog?.close();
  await browser?.close();
  server.kill('SIGTERM');
  await new Promise(r => server.once('exit', r));
  await log.close();
  await chmod(root + '/blocked', 0o700);
  console.log('Evidence:', root);
}
