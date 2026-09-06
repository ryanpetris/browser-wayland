// Run in the Docker rig with a headed display at :95 and the release binary.
// Chromium check also needs wev; Firefox check needs geckodriver listening on port 4445.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {mkdir, mkdtemp, open, readFile, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';

const root = await mkdtemp(tmpdir() + '/bw-pip-probe-');
await mkdir(root + '/runtime', {mode: 0o700});
const log = await open(root + '/server.log', 'w');
const origin = 'http://127.0.0.1:8093';
const server = spawn(
    '/src/target/release/browser-wayland',
    [
      '--no-audio', '--no-rtc', '--no-tls', '--render-node', 'none', '--codec', 'vp8', '--listen',
      '127.0.0.1:8093', '--socket-name', 'wayland-pip-probe'
    ],
    {
      env: {
        ...process.env,
        HOME: root,
        XDG_CONFIG_HOME: root + '/config',
        XDG_RUNTIME_DIR: root + '/runtime',
        RUST_LOG: 'bw_server::api=debug'
      },
      stdio: ['ignore', log.fd, log.fd],
    });
const wait = async fn => {
  for (let i = 0; i < 200; i++) {
    if (await fn()) return;
    await new Promise(r => setTimeout(r, 100));
  }
  throw new Error('timed out');
};
let session;
const wd = async (path, body, method = body ? 'POST' : 'GET') => {
  const r = await fetch(
      'http://127.0.0.1:4445' + path,
      {method, headers: {'Content-Type': 'application/json'}, body: body ? JSON.stringify(body) : undefined});
  const data = await r.json();
  if (!r.ok) throw new Error(`${data.value?.error}: ${data.value?.message}`);
  return data.value;
};

try {
  await wait(async () => {
    try {
      return (await fetch(origin)).ok && !!await readFile(root + '/config/browser-wayland/token')
    } catch {
      return false
    }
  });
  const token = (await readFile(root + '/config/browser-wayland/token', 'utf8')).trim();
  session = (await wd('/session', {
              capabilities: {
                alwaysMatch: {
                  browserName: 'firefox',
                  'moz:firefoxOptions':
                      {binary: '/usr/bin/firefox', prefs: {'browser.shell.checkDefaultBrowser': false}}
                }
              }
            })).sessionId;
  const route = '/session/' + session;
  const js = script => wd(route + '/execute/sync', {script, args: []});
  const click = async selector => {
    const el = await wd(route + '/element', {using: 'css selector', value: selector});
    await wd(route + '/element/' + Object.values(el)[0] + '/click', {});
  };
  await wd(route + '/url', {url: origin + '/#token=' + token});
  await wait(() => js('return !!window.bw?.store.get().stream'));
  await wd(route + '/execute/sync', {
    script: 'bw.spawn(arguments[0])',
    args: [
      'foot --app-id=pip-firefox sh -c \'while true; do date +%s%N; sleep 0.2; done & while read line; do echo "$line" >> ' +
      root + '/typed; done\''
    ]
  });
  await wait(() => js('return bw.store.get().windows.some(w=>w.app_id==="pip-firefox")'));
  await click('canvas');
  await js(
      'document.addEventListener("paste",e=>window.pasteSeen={types:[...e.clipboardData.types],text:e.clipboardData.getData("text/plain")});return navigator.clipboard.writeText("normal firefox clipboard")');
  await wd(route + '/actions', {
    actions: [{
      type: 'key',
      id: 'keyboard',
      actions: [
        {type: 'keyDown', value: '\uE009'}, {type: 'keyDown', value: '\uE008'}, {type: 'keyDown', value: 'v'},
        {type: 'keyUp', value: 'v'}, {type: 'keyUp', value: '\uE008'}, {type: 'keyUp', value: '\uE009'}
      ]
    }]
  });
  await new Promise(r => setTimeout(r, 300));
  console.log(
      'Firefox normal clipboard', await js('return bw.clipboard.read()'),
      await js('return window.pasteSeen??null'));
  await click('button[title="Picture-in-picture"]');
  await wait(
      () => js(
          'return !!documentPictureInPicture.window?.document.querySelector("iframe")?.contentWindow.bw?.store.get().stream'));
  console.log(
      'Firefox PiP',
      await js(
          'const w=documentPictureInPicture.window.document.querySelector("iframe").contentWindow; return {status:w.bw.store.get().status,role:w.bw.store.get().role,hidden:w.document.hidden,frames:w.bw().videoSeq,parentRole:bw.store.get().role}'));
  await wait(() => js('return bw.store.get().role==="participant"'));
  const before = await js(
      'return documentPictureInPicture.window.document.querySelector("iframe").contentWindow.bw().videoSeq');
  const handles = await wd(route + '/window/handles');
  console.log('Firefox handles', handles.length);
  const main = await wd(route + '/window');
  const pipHandle = handles.find(h => h !== main);
  if (pipHandle) {
    await wd(route + '/window', {handle: pipHandle});
    await wd(route + '/frame', {id: 0});
    await click('canvas');
    await wd(route + '/actions', {
      actions: [{
        type: 'key',
        id: 'keyboard',
        actions: Array.from('firefox input')
                     .flatMap(value => [{type: 'keyDown', value}, {type: 'keyUp', value}])
                     .concat([{type: 'keyDown', value: '\uE007'}, {type: 'keyUp', value: '\uE007'}])
      }]
    });
    await wait(async () => {
      try {
        return (await readFile(root + '/typed', 'utf8')).includes('firefox input')
      } catch {
        return false
      }
    });
    console.log('Firefox real keyboard input');
    await js(
        'document.addEventListener("paste",e=>window.pasteSeen={types:[...e.clipboardData.types],text:e.clipboardData.getData("text/plain")})');
    console.log(
        'Firefox clipboard write',
        await js(
            'return navigator.clipboard.writeText("firefox clipboard").then(()=>"ok").catch(e=>e.name)'));
    await wd(route + '/actions', {
      actions: [{
        type: 'key',
        id: 'keyboard',
        actions: [
          {type: 'keyDown', value: '\uE009'}, {type: 'keyDown', value: '\uE008'},
          {type: 'keyDown', value: 'v'}, {type: 'keyUp', value: 'v'}, {type: 'keyUp', value: '\uE008'},
          {type: 'keyUp', value: '\uE009'}
        ]
      }]
    });
    await new Promise(r => setTimeout(r, 500));
    console.log('Firefox clipboard received', await js('return bw.clipboard.read()'));
    console.log('Firefox paste event', await js('return window.pasteSeen??null'));
    await click('button[title="On-screen keyboard"]');
    await js(
        'document.querySelector("[data-keyboard]").dispatchEvent(new CompositionEvent("compositionend",{data:"firefox composition",bubbles:true}));bw.key("Return")');
    await wait(async () => {
      try {
        return (await readFile(root + '/typed', 'utf8')).includes('firefox composition')
      } catch {
        return false
      }
    });
    console.log('Firefox composition commit');
    await click('button[aria-label="Hide the keyboard row"]');
    await click('canvas');
    await wd(
        route + '/execute/sync',
        {script: 'bw.spawn(arguments[0])', args: ['stdbuf -oL wev > ' + root + '/wev.log']});
    await wait(() => js('return bw.store.get().windows.some(w=>w.app_id==="wev")'));
    await js(
        'const w=bw.store.get().windows.find(w=>w.app_id==="wev");bw.control({op:"maximize",id:w.id});bw.activate(w.id)');
    await click('canvas');
    await wd(route + '/actions', {
      actions: [{
        type: 'wheel',
        id: 'wheel',
        actions: [{type: 'scroll', x: 100, y: 100, deltaX: 0, deltaY: 90, duration: 100, origin: 'viewport'}]
      }]
    });
    await wait(async () => {
      try {
        const log = await readFile(root + '/wev.log', 'utf8');
        return log.includes('button:') && log.includes('axis:')
      } catch {
        return false
      }
    });
    console.log('Firefox Wayland client receives button and axis');
    await js(
        'const el=document.querySelector("canvas"),data=new DataTransfer();data.items.add(new File(["firefox drop"],"firefox-drop.txt"));for(const type of ["dragenter","dragover","drop"])el.dispatchEvent(new DragEvent(type,{dataTransfer:data,bubbles:true,cancelable:true,clientX:100,clientY:100}))');
    await wait(() => js('return bw.store.get().notice?.text.includes("transfer folder")'));
    console.log('Firefox unclaimed file drop');
    console.log(
        'Firefox pointer lock',
        await js(
            'return document.querySelector("canvas").requestPointerLock().then(()=>!!document.pointerLockElement).catch(e=>e.name)'));
    await js('document.exitPointerLock();document.dispatchEvent(new Event("pointerlockerror"))');
    await wait(() => js('return bw.store.get().notice?.text.startsWith("Pointer capture failed.")'));
    console.log('Firefox pointer lock error event reports failure');
    await js(
        'const w=bw.store.get().windows.find(w=>w.app_id==="wev");if(w)bw.control({op:"close",id:w.id})');
    await wd(route + '/window', {handle: main});
  }
  await wd(route + '/window/minimize', {});
  await new Promise(r => setTimeout(r, 1500));
  const minFrame = await js(
      'return documentPictureInPicture.window.document.querySelector("iframe").contentWindow.bw().videoSeq');
  assert.notEqual(minFrame, before);
  console.log('Firefox minimized frames', before, minFrame);
  await wd(route + '/window/rect', {width: 1200, height: 900});

  const tab = await wd(route + '/window/new', {type: 'tab'});
  await wd(route + '/window', {handle: tab.handle});
  await wd(route + '/url', {url: 'about:blank'});
  await new Promise(r => setTimeout(r, 1500));
  // Read the retained parent window from a same-origin sibling tab while the opener stays hidden.
  await wd(route + '/window', {handle: main});
  const after = await js(
      'return documentPictureInPicture.window.document.querySelector("iframe").contentWindow.bw().videoSeq');
  assert.notEqual(after, minFrame);
  console.log('Firefox background frames', before, after);
  await js('bw.pip.close()');
  await wait(() => js('return bw.store.get().role==="controller"'));
  console.log('Firefox return control');
  await js(
      'const b=document.createElement("button");b.id="window-pip";b.textContent="Window PiP";b.style="position:fixed;top:0;left:0;z-index:9999";b.onclick=()=>bw.pip.open(bw.store.get().windows.find(w=>w.app_id==="pip-firefox").id);document.body.append(b)');
  await click('#window-pip');
  await wait(
      () => js(
          'return !!documentPictureInPicture.window?.document.querySelector("iframe")?.contentWindow.bw?.store.get().stream'));
  const winHandles = await wd(route + '/window/handles');
  const winPip = winHandles.find(h => h !== main && h !== tab.handle);
  assert.ok(winPip);
  await wd(route + '/window', {handle: winPip});
  await wd(route + '/frame', {id: 0});
  await click('canvas');
  await wd(route + '/actions', {
    actions: [{
      type: 'key',
      id: 'keyboard',
      actions: Array.from('firefox window')
                   .flatMap(value => [{type: 'keyDown', value}, {type: 'keyUp', value}])
                   .concat([{type: 'keyDown', value: '\uE007'}, {type: 'keyUp', value: '\uE007'}])
    }]
  });
  await wait(async () => {
    try {
      return (await readFile(root + '/typed', 'utf8')).includes('firefox window')
    } catch {
      return false
    }
  });
  console.log('Firefox window PiP real keyboard');
  await click('button[title="Return to main viewer"]');
  await wd(route + '/window', {handle: main});

} finally {
  if (session) await wd('/session/' + session, undefined, 'DELETE');
  server.kill('SIGTERM');
  await new Promise(r => server.exitCode != null ? r() : server.once('exit', r));
  await log.close();
  await rm(root, {recursive: true, force: true, maxRetries: 5, retryDelay: 100})
}
