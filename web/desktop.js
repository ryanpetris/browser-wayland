// Desktop UI on top of the video: the window list and click-to-activate.
// Fed by the WINDOWS message; talks back with CONTROL (see crates/bw-server/src/protocol.rs).

let windows = [];
let sendControl = () => {};
const panel = document.getElementById('panel'), wins = document.getElementById('wins'), spawn = document.getElementById('spawn');

export const getWindows = () => windows;
export const control = obj => sendControl(obj);

export function initDesktop(send) {
  sendControl = send;
  document.getElementById('panelbtn').onclick = () => panel.classList.toggle('open');
  spawn.onkeydown = e => {
    if (e.key === 'Enter' && spawn.value.trim()) { sendControl({ op: 'spawn', cmd: spawn.value.trim() }); spawn.value = ''; }
    if (e.key === 'Escape') spawn.blur();
    e.stopPropagation();
  };
}

export function onWindows(list) {
  windows = list;
  renderList();
}

// One hue per app id, so every window of an app gets the same colour.
function hue(s) {
  let h = 0;
  for (const c of s) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return h % 360;
}
export const color = w => `hsl(${hue(w.app_id || w.title)} 70% 55%)`;

function renderList() {
  const order = windows.slice().sort((a, b) => (a.minimized - b.minimized) || (b.z - a.z)); // top-most first, minimized last
  wins.replaceChildren(...order.map(w => {
    const row = document.createElement('div');
    row.className = 'win' + (w.focused ? ' focused' : '');
    row.innerHTML = '<span class="dot"></span><span class="title"></span><span class="badge"></span>'
      + '<button title="Maximize / restore">⤢</button><button title="Minimize / restore">⌄</button><button title="Close">✕</button>';
    row.querySelector('.dot').style.background = color(w);
    const title = row.querySelector('.title');
    title.textContent = w.title || w.app_id || `#${w.id}`;
    title.title = `${w.app_id}${w.pid ? ' · pid ' + w.pid : ''} · ${w.w}×${w.h} at ${w.x},${w.y}`;
    row.querySelector('.badge').textContent = [w.fullscreen && 'full', w.maximized && 'max', w.minimized && 'min'].filter(Boolean).join(' ');
    const [max, min, close] = row.querySelectorAll('button');
    row.onclick = () => sendControl({ id: w.id, op: 'activate' });
    max.onclick = e => { e.stopPropagation(); sendControl({ id: w.id, op: w.maximized ? 'unmaximize' : 'maximize' }); };
    min.onclick = e => { e.stopPropagation(); sendControl({ id: w.id, op: w.minimized ? 'unminimize' : 'minimize' }); };
    close.onclick = e => { e.stopPropagation(); sendControl({ id: w.id, op: 'close' }); };
    return row;
  }));
}
