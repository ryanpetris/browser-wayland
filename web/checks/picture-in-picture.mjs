// Run in the Docker rig with a headed display at :95 and the release binary.
// Chromium check also needs wev; Firefox check needs geckodriver listening on port 4445.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {mkdir, mkdtemp, open, readFile, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {chromium} from 'playwright-core';

const root = await mkdtemp(tmpdir() + '/bw-pip-probe-');
await mkdir(root + '/runtime', {mode: 0o700});
const log = await open(root + '/server.log', 'w');
const origin = 'http://127.0.0.1:8093';
const server = spawn(
    '/src/target/release/browser-wayland',
    [
      '--no-rtc', '--no-tls', '--render-node', 'none', '--codec', 'vp8', '--listen', '127.0.0.1:8093',
      '--socket-name', 'wayland-pip-probe'
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
let browser;
try {
  await wait(async () => {
    try {
      return (await fetch(origin)).ok && !!await readFile(root + '/config/browser-wayland/token')
    } catch {
      return false
    }
  });
  const token = (await readFile(root + '/config/browser-wayland/token', 'utf8')).trim();
  browser = await chromium.launch({
    headless: false,
    executablePath: '/usr/bin/chromium',
    env: {...process.env, DISPLAY: ':95', XDG_CONFIG_HOME: root + '/chromium'},
    args: [
      '--no-sandbox', '--autoplay-policy=no-user-gesture-required', '--use-fake-device-for-media-stream',
      '--use-fake-ui-for-media-stream'
    ]
  });
  const context = await browser.newContext();
  await context.addInitScript(() => {
    const media = window.matchMedia;
    window.testMediaQueries = [];
    window.matchMedia = query => {
      const result = media.call(window, query);
      if (query.startsWith('(resolution:')) testMediaQueries.push(result);
      return result;
    };
    const Socket = WebSocket;
    window.testPackets = [];
    window.WebSocket = class extends Socket {
      constructor(...args) {
        super(...args);
        window.testSocket = this
      }
      send(data) {
        if (data instanceof ArrayBuffer || ArrayBuffer.isView(data))
          window.testPackets.push(
              Array.from(new Uint8Array(data instanceof ArrayBuffer ? data : data.buffer)));
        super.send(data)
      }
    }
  });
  const page = await context.newPage();
  await page.goto(origin + '/#token=' + token);
  await page.waitForFunction(() => !!window.bw?.store.get().stream);
  await page.evaluate(() => bw.takeControl());
  await page.evaluate(
      () => bw.spawn(
          'gst-launch-1.0 -q audiotestsrc is-live=true freq=440 volume=0.1 ! audioconvert ! audio/x-raw,rate=48000,channels=2 ! pipewiresink sync=false'));
  await page.waitForFunction(() => !!bw.store.get().playback);
  await page.evaluate(() => bw.mic.start());
  await page.waitForFunction(() => bw.store.get().mic);
  await page.evaluate(
      cmd => bw.spawn(cmd),
      'foot --app-id=pip-probe sh -c ' +
          '\'while true; do date +%s%N; sleep 0.2; done & while read line; do echo "$line" >> ' + root +
          '/typed; done\'');
  await page.waitForFunction(() => bw.store.get().windows.some(w => w.app_id === 'pip-probe'));
  const id = await page.evaluate(() => bw.store.get().windows.find(w => w.app_id === 'pip-probe').id);
  const nextPage = context.waitForEvent('page');
  await page.getByRole('button', {name: 'Picture-in-picture', exact: true}).first().click();
  const pipPage = await nextPage;
  await pipPage.waitForTimeout(300);
  let frame = pipPage.frames().find(f => f.parentFrame());
  await frame.waitForFunction(() => window.bw?.store.get().role === 'controller' && !!bw.store.get().stream);
  await page.waitForFunction(() => window.bw?.store.get().role === 'participant');
  await frame.waitForFunction(() => !!bw.store.get().playback);
  assert.equal(await page.evaluate(() => bw.store.get().playback), null);
  assert.equal(await page.evaluate(() => bw.store.get().mic), false);
  assert.equal(await frame.evaluate(() => bw.store.get().mic), false);
  console.log('single audio owner; old microphone stopped, child microphone off');
  await frame.locator('canvas').click();
  await pipPage.keyboard.type('desktop input');
  await pipPage.keyboard.press('Enter');
  await wait(async () => {
    try {
      return (await readFile(root + '/typed', 'utf8')).includes('desktop input')
    } catch {
      return false
    }
  });
  const before = await frame.evaluate(() => bw().videoSeq);
  const other = await context.newPage();
  await other.goto(origin + '/#token=' + token);
  await other.waitForFunction(() => window.bw?.store.get().role === 'participant');
  await other.bringToFront();
  await frame.waitForFunction(before => bw().videoSeq !== before, before);
  const cdp = await context.newCDPSession(page);
  const {windowId} = await cdp.send('Browser.getWindowForTarget');
  const preMin = await frame.evaluate(() => bw().videoSeq);
  await cdp.send('Browser.setWindowBounds', {windowId, bounds: {windowState: 'minimized'}});
  await frame.waitForTimeout(1500);
  await frame.waitForFunction(n => bw().videoSeq !== n, preMin);
  console.log('minimized opener still renders', await frame.evaluate(() => document.hidden));
  await cdp.send('Browser.setWindowBounds', {windowId, bounds: {windowState: 'normal'}});
  console.log('desktop PiP background', await frame.evaluate(() => ({
                                                               status: bw.store.get().status,
                                                               role: bw.store.get().role,
                                                               hidden: document.hidden,
                                                               frames: bw().videoSeq
                                                             })));
  await other.evaluate(() => bw.takeControl());
  await other.waitForFunction(() => window.bw?.store.get().role === 'controller');
  await frame.getByRole('button', {name: 'Return to main viewer'}).click();
  await wait(() => Promise.resolve(pipPage.isClosed()));
  assert.equal(await other.evaluate(() => window.bw?.store.get().role), 'controller');
  assert.equal(await page.evaluate(() => window.bw?.store.get().role), 'participant');
  console.log('return preserves third-party controller');
  await page.bringToFront();
  const mainClaimNext = context.waitForEvent('page');
  await page.getByRole('button', {name: 'Picture-in-picture', exact: true}).first().click();
  const claimPip = await mainClaimNext;
  await claimPip.waitForTimeout(300);
  await page.bringToFront();
  await page.getByRole('button', {name: 'Take control', exact: true}).click();
  await wait(() => Promise.resolve(claimPip.isClosed()));
  await page.waitForFunction(() => window.bw?.store.get().role === 'controller');
  console.log('opener Take control returns desktop presentation');

  await page.bringToFront();
  await page.evaluate(() => bw.takeControl());
  await page.waitForFunction(() => window.bw?.store.get().role === 'controller');
  const next = context.waitForEvent('page');
  await page.getByRole('button', {name: 'Picture-in-picture', exact: true}).first().click();
  const pip2 = await next;
  await pip2.waitForTimeout(300);
  frame = pip2.frames().find(f => f.parentFrame());
  await frame.waitForFunction(() => window.bw?.store.get().role === 'controller');
  await frame.getByRole('button', {name: 'Return to main viewer'}).click();
  await page.waitForFunction(() => window.bw?.store.get().role === 'controller');
  await page.waitForFunction(() => !!bw.store.get().playback);
  assert.equal(await page.evaluate(() => bw.store.get().mic), false);
  console.log('return restores opener controller and playback without microphone');
  await page.evaluate(id => {
    const b = document.createElement('button');
    b.id = 'open-window-pip';
    b.textContent = 'Open test window';
    b.onclick = () => bw.pip.open(id);
    document.body.append(b);
    b.style = 'position:fixed;top:0;left:0;z-index:9999'
  }, id);
  const nextWindow = context.waitForEvent('page');
  await page.click('#open-window-pip');
  const pip3 = await nextWindow;
  await pip3.waitForTimeout(300);
  frame = pip3.frames().find(f => f.parentFrame());
  await frame.waitForFunction(() => !!window.bw?.store.get().stream);
  await frame.locator('canvas').click();
  await pip3.keyboard.type('window input');
  await pip3.keyboard.press('Enter');
  await wait(async () => {
    try {
      return (await readFile(root + '/typed', 'utf8')).includes('window input')
    } catch {
      return false
    }
  });
  console.log('window PiP input');
  await page.bringToFront();
  await page.getByRole('button', {name: 'Picture-in-picture', exact: true}).first().click();
  await pip3.waitForTimeout(300);
  assert.equal(pip3.isClosed(), false);
  frame = pip3.frames().find(f => f.parentFrame());
  await frame.waitForFunction(
      () => window.bw?.store.get().role === 'controller' && bw.store.get().sessionId != null);
  console.log('window to desktop reuses PiP');
  await frame.locator('canvas').click();
  await pip3.keyboard.press('Control+Shift+V');
  await frame.evaluate(() => {
    const data = new DataTransfer();
    data.setData('text/plain', 'pip clipboard');
    document.dispatchEvent(
        new ClipboardEvent('paste', {clipboardData: data, bubbles: true, cancelable: true}))
  });
  await frame.waitForFunction(async () => await bw.clipboard.read() === 'pip clipboard');
  console.log('PiP clipboard event reaches remote clipboard');
  await frame.getByRole('button', {name: 'On-screen keyboard', exact: true}).click();
  await frame.locator('[data-keyboard]')
      .evaluate(
          el => el.dispatchEvent(
              new CompositionEvent('compositionend', {data: 'composition check', bubbles: true})));
  await frame.locator('[data-keyboard]').press('Enter');
  await wait(async () => {
    try {
      return (await readFile(root + '/typed', 'utf8')).includes('composition check')
    } catch {
      return false
    }
  });
  await frame.getByRole('button', {name: 'Hide the keyboard row'}).click();
  console.log('PiP composition commit reaches terminal through keyboard row');
  await frame.locator('canvas').evaluate(async el => {
    try {
      await el.requestPointerLock();
      window.lockResult = !!document.pointerLockElement
    } catch (e) {
      window.lockResult = e.name
    }
  });
  console.log('Chromium pointer lock', await frame.evaluate(() => window.lockResult));
  await frame.evaluate(() => document.exitPointerLock());
  await frame.evaluate(cmd => bw.spawn(cmd), 'stdbuf -oL wev > ' + root + '/wev.log');
  await frame.waitForFunction(() => bw.store.get().windows.some(w => w.app_id === 'wev'));
  await frame.evaluate(() => {
    const w = bw.store.get().windows.find(w => w.app_id === 'wev');
    bw.control({op: 'maximize', id: w.id});
    bw.activate(w.id)
  });
  await frame.locator('canvas').click({position: {x: 100, y: 100}});
  await pip3.mouse.wheel(0, 90);
  await frame.evaluate(() => bw.setTouchMouse(false));
  const touchSession = await context.newCDPSession(pip3);
  await touchSession.send('Input.dispatchTouchEvent', {type: 'touchStart', touchPoints: [{x: 100, y: 100}]});
  await touchSession.send('Input.dispatchTouchEvent', {type: 'touchEnd', touchPoints: []});
  assert.ok(await frame.evaluate(() => testPackets.some(p => p[0] === 0x86)));
  assert.ok(await frame.evaluate(() => testPackets.some(p => p[0] === 0x92)));
  console.log('wheel and touch events emit protocol input');
  await wait(async () => {
    try {
      const log = await readFile(root + '/wev.log', 'utf8');
      return log.includes('button:') && log.includes('axis:') && log.includes('wl_touch')
    } catch {
      return false
    }
  });
  console.log('Wayland client receives button, axis and touch');
  await frame.locator('canvas').evaluate(el => {
    const data = new DataTransfer();
    data.items.add(new File(['pip file'], 'pip-drop.txt', {type: 'text/plain'}));
    for (const type of ['dragenter', 'dragover', 'drop'])
      el.dispatchEvent(new DragEvent(
          type, {dataTransfer: data, bubbles: true, cancelable: true, clientX: 100, clientY: 100}))
  });
  await frame.waitForFunction(() => !bw.store.get().upload);
  await frame.waitForTimeout(500);
  await frame.waitForFunction(() => bw.store.get().notice?.text.includes('transfer folder'));
  console.log('PiP unclaimed drop saved to transfer folder');

  await page.evaluate(() => bw.pip.close());
  await page.waitForFunction(() => window.bw?.store.get().role === 'controller');
  const viewerToken = (await readFile(root + '/config/browser-wayland/viewer-token', 'utf8')).trim();
  const readOnly = await context.newPage();
  await readOnly.goto(origin + '/#token=' + viewerToken);
  await readOnly.waitForFunction(() => window.bw?.store.get().role === 'viewer');
  const roId = await readOnly.evaluate(() => bw.store.get().sessionId);
  await page.evaluate(id => bw.handoff(id), roId);
  await readOnly.evaluate(() => bw.handoff(1n));
  await page.waitForTimeout(200);
  assert.equal(await page.evaluate(() => window.bw?.store.get().role), 'controller');
  const roNext = context.waitForEvent('page');
  await readOnly.getByRole('button', {name: 'Picture-in-picture', exact: true}).first().click();
  const roPip = await roNext;
  await roPip.waitForTimeout(300);
  const roFrame = roPip.frames().find(f => f.parentFrame());
  await roFrame.waitForFunction(() => window.bw?.store.get().role === 'viewer');
  await roFrame.evaluate(() => {
    bw.takeControl();
    bw.handoff(1n)
  });
  await page.waitForTimeout(300);
  assert.equal(await roFrame.evaluate(() => window.bw?.store.get().role), 'viewer');
  assert.equal(await page.evaluate(() => window.bw?.store.get().role), 'controller');
  assert.equal(await roFrame.getByRole('button', {name: 'Take control', exact: true}).count(), 0);
  await readOnly.evaluate(() => bw.pip.close());
  console.log('read-only PiP stays read-only');
  await page.evaluate(() => {
    window.originalPipRequest = documentPictureInPicture.requestWindow;
    documentPictureInPicture.requestWindow = () =>
        Promise.reject(new DOMException('Denied', 'NotAllowedError'))
  });
  await page.bringToFront();
  await page.getByRole('button', {name: 'Picture-in-picture', exact: true}).first().click();
  await page.waitForFunction(() => bw.store.get().notice?.text.includes('could not open'));
  assert.equal(await page.evaluate(() => window.bw?.store.get().role), 'controller');
  await page.evaluate(() => documentPictureInPicture.requestWindow = window.originalPipRequest);
  console.log('rejected request preserves viewer');
  const unsupported = await context.newPage();
  await unsupported.addInitScript(
      () => Object.defineProperty(window, 'documentPictureInPicture', {value: undefined}));
  await unsupported.goto(origin + '/#token=' + token);
  await unsupported.waitForFunction(() => !!window.bw?.store.get().stream);
  assert.equal(await unsupported.getByRole('button', {name: 'Picture-in-picture', exact: true}).count(), 0);
  assert.equal(
      await unsupported
          .getByRole('button', {name: 'Fullscreen (browser shortcuts go to the desktop)', exact: true})
          .count(),
      1);
  await unsupported.close();
  const popNext = context.waitForEvent('page');
  await page.evaluate(id => window.open('/?window=' + id, 'normal-test', 'popup,width=500,height=400'), id);
  const normal = await popNext;
  await normal.waitForFunction(() => !!window.bw?.store.get().stream);
  await normal.close();
  console.log('unsupported API and ordinary popup');
  await page.bringToFront();
  await page.getByRole('button', {name: 'Fullscreen (browser shortcuts go to the desktop)', exact: true})
      .click();
  await page.waitForFunction(() => !!document.fullscreenElement);
  await page.evaluate(() => document.exitFullscreen());
  const reopen = context.waitForEvent('page');
  await page.getByRole('button', {name: 'Picture-in-picture', exact: true}).first().click();
  const lifePip = await reopen;
  await lifePip.waitForTimeout(300);
  let lifeFrame = lifePip.frames().find(f => f.parentFrame());
  await lifeFrame.waitForFunction(() => window.bw?.store.get().role === 'controller');
  const oldId = await lifeFrame.evaluate(() => bw.store.get().sessionId);
  await lifeFrame.evaluate(() => testSocket.close());
  await lifeFrame.waitForFunction(
      old => bw.store.get().status === 'connected' && bw.store.get().sessionId !== old, oldId);
  await lifeFrame.waitForFunction(() => window.bw?.store.get().role === 'controller');
  console.log('child socket reconnect restores presentation control');
  const lifeCdp = await context.newCDPSession(lifePip);
  const lifeWin = await lifeCdp.send('Browser.getWindowForTarget');
  await lifeCdp.send(
      'Browser.setWindowBounds', {windowId: lifeWin.windowId, bounds: {width: 480, height: 360}});
  await lifeFrame.waitForFunction(
      () => bw.store.get().stream.width ===
          Math.round(document.querySelector('canvas').getBoundingClientRect().width * devicePixelRatio));
  console.log('PiP viewport drives output');
  for (const deviceScaleFactor of [1.5, 2, 1]) {
    await lifeCdp.send(
        'Emulation.setDeviceMetricsOverride', {width: 480, height: 360, deviceScaleFactor, mobile: false});
    // CDP changes iframe DPR without a media-query event when CSS dimensions stay fixed.
    // Supply that event to exercise the viewer's rearm; viewport resizing is checked above.
    await lifeFrame.evaluate(() => {
      const query = testMediaQueries.at(-1);
      if (!query.matches) query.dispatchEvent(new Event('change'));
    });
    await lifeFrame.waitForFunction(
        dpr => devicePixelRatio === dpr && Math.abs(bw.store.get().stream.scale - dpr) < 0.01,
        deviceScaleFactor);
  }
  console.log('PiP DPR variants and media-query rearm');
  await lifeFrame.evaluate(() => document.dispatchEvent(new Event('pointerlockerror')));
  await lifeFrame.waitForFunction(() => bw.store.get().notice?.text.startsWith('Pointer capture failed.'));
  console.log('pointer lock error event reports failure');
  await lifeFrame.evaluate(() => bw.activate(bw.store.get().windows.find(w => w.app_id === 'wev').id));
  const inputOffset = (await readFile(root + '/wev.log', 'utf8')).length;
  await lifeFrame.locator('canvas').click();
  await lifePip.keyboard.down('Shift');
  await wait(async () => {
    const tail = (await readFile(root + '/wev.log', 'utf8')).slice(inputOffset);
    return tail.includes('Shift_L')
  });
  await page.evaluate(() => bw.pip.close());
  await page.waitForFunction(() => window.bw?.store.get().role === 'controller');
  await wait(async () => {
    const tail = (await readFile(root + '/wev.log', 'utf8')).slice(inputOffset);
    return tail.includes('Shift_L') && tail.includes('state: 0 (released)')
  });
  console.log('held key released on return');
  const goneNext = context.waitForEvent('page');
  await page.click('#open-window-pip');
  const gonePip = await goneNext;
  await gonePip.waitForTimeout(300);
  await page.evaluate(id => bw.control({op: 'close', id}), id);
  await wait(() => Promise.resolve(gonePip.isClosed()));
  console.log('remote window closure closes PiP');
  const navNext = context.waitForEvent('page');
  await page.getByRole('button', {name: 'Picture-in-picture', exact: true}).first().click();
  const navPip = await navNext;
  await navPip.waitForTimeout(300);
  await page.goto(origin + '/?navigation-check=1#token=' + token);
  await wait(() => Promise.resolve(navPip.isClosed()));
  await page.waitForFunction(() => !!window.bw?.store.get().stream);
  console.log('opener navigation closes PiP');
  await page.evaluate(() => {
    window.dispatchEvent(new PageTransitionEvent('pagehide', {persisted: true}));
    window.dispatchEvent(new PageTransitionEvent('pageshow', {persisted: true}))
  });
  const authNext = context.waitForEvent('page');
  await page.getByRole('button', {name: 'Picture-in-picture', exact: true}).first().click();
  const authPip = await authNext;
  await authPip.waitForTimeout(300);
  const rotated = await fetch(
      origin + '/api/token/rotate', {method: 'POST', headers: {Authorization: 'Bearer ' + token}});
  assert.equal(rotated.status, 200);
  await wait(() => Promise.resolve(authPip.isClosed()));
  await page.waitForFunction(() => bw.store.get().status === 'unauthorized');
  console.log('token rotation closes PiP');



} finally {
  await browser?.close();
  server.kill('SIGTERM');
  await new Promise(r => server.exitCode != null ? r() : server.once('exit', r));
  await log.close();
  await rm(root, {recursive: true, force: true, maxRetries: 5, retryDelay: 100})
}
