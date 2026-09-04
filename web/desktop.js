// Desktop UI on top of the video: the window list, click-to-activate, colour-coded borders.
// Fed by the WINDOWS message; talks back with CONTROL (see crates/bw-server/src/protocol.rs).

let windows = [];
let sendControl = () => {}, streamSize = () => null, releaseInput = () => {}; // streamSize(): logical {w, h} of the video, or null before Config
const rows = new Map(); // window id -> its panel row, kept across list updates so thumbnails only reload when their URL changes
let thumbQueue = Promise.resolve(); // the server renders one snapshot at a time (429 otherwise), so fetch them one by one
const panel = document.getElementById('panel'), wins = document.getElementById('wins'), spawn = document.getElementById('spawn');
const overlay = document.getElementById('overlay'), canvas = document.getElementById('c');

export const getWindows = () => windows;
export const control = obj => sendControl(obj);
const TOKEN = new URLSearchParams(location.search).get('token') ?? '';
/// fetch() with the bearer token, for the HTTP API.
export const api = (path, init = {}) => fetch(path, { ...init, headers: { ...init.headers, Authorization: `Bearer ${TOKEN}` } });
export const snapshotUrl = (id, scale = 1) => `${id == null ? '/api/screenshot.png' : `/api/windows/${id}/snapshot.png`}?scale=${scale}`;
export const snapshot = async (id, scale = 1) => (await api(snapshotUrl(id, scale))).blob();

export function initDesktop(send, size, release) {
  sendControl = send;
  streamSize = size;
  releaseInput = release;
  spawn.onfocus = () => releaseInput(); // a key held on the canvas must not stay held in the compositor
  document.getElementById('panelbtn').onclick = () => { panel.classList.toggle('open'); renderList(); };
  const borders = document.getElementById('borders');
  try { overlay.hidden = localStorage.getItem('bw.borders') !== '1'; } catch {}
  borders.onclick = () => {
    overlay.hidden = !overlay.hidden;
    try { localStorage.setItem('bw.borders', overlay.hidden ? '0' : '1'); } catch {}
    renderBorders();
  };
  window.addEventListener('resize', renderBorders);
  spawn.onkeydown = e => {
    if (e.key === 'Enter' && spawn.value.trim()) { sendControl({ op: 'spawn', cmd: spawn.value.trim() }); spawn.value = ''; }
    if (e.key === 'Escape') spawn.blur();
    e.stopPropagation();
  };
}

export function onWindows(list) {
  windows = list;
  renderList();
  renderBorders();
}

// One rectangle per visible window, in CSS px over the canvas; the same hue as the list.
export function renderBorders() {
  if (overlay.hidden) return;
  const size = streamSize();
  if (!size) return;
  const r = canvas.getBoundingClientRect();
  const sx = r.width / size.w, sy = r.height / size.h;
  overlay.replaceChildren(...windows.filter(w => !w.minimized).map(w => {
    const d = document.createElement('div');
    d.className = w.focused ? 'focused' : '';
    d.style.cssText = `left:${r.left + w.x * sx}px;top:${r.top + w.y * sy}px;width:${w.w * sx}px;height:${w.h * sy}px;border-color:${color(w)}`;
    const label = document.createElement('span');
    label.textContent = w.app_id || w.title;
    label.style.background = color(w);
    d.append(label);
    return d;
  }));
}

// One hue per app id, so every window of an app gets the same colour.
function hue(s) {
  let h = 0;
  for (const c of s) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return h % 360;
}
export const color = w => `hsl(${hue(w.app_id || w.title)} 70% 55%)`;

function renderList() {
  if (!panel.classList.contains('open')) return; // a hidden <img> still fetches its thumbnail
  const order = windows.slice().sort((a, b) => (a.minimized - b.minimized) || (b.z - a.z)); // top-most first, minimized last
  for (const id of rows.keys()) if (!windows.some(w => w.id === id)) rows.delete(id);
  wins.replaceChildren(...order.map(w => {
    let row = rows.get(w.id);
    if (!row) {
      row = document.createElement('div');
      row.innerHTML = '<img class="thumb"><span class="dot"></span><span class="title"></span><span class="badge"></span>'
        + '<button title="Snapshot (PNG)">📷</button><button title="Maximize / restore">⤢</button><button title="Minimize / restore">⌄</button><button title="Close">✕</button>';
      row.querySelector('.dot').style.background = color(w);
      rows.set(w.id, row);
    }
    row.className = 'win' + (w.focused ? ' focused' : '');
    // updated_ms has whole-second resolution, so a busy window costs at most one render per second.
    // <img> can't send the bearer header, so the PNG comes through fetch() and a blob URL.
    const thumb = row.querySelector('.thumb');
    if (thumb.dataset.key !== String(w.updated_ms)) {
      thumb.dataset.key = w.updated_ms;
      thumbQueue = thumbQueue.then(() => snapshot(w.id, 0.12)).then(b => {
        if (thumb.src.startsWith('blob:')) URL.revokeObjectURL(thumb.src);
        thumb.src = URL.createObjectURL(b);
      }).catch(() => {});
    }
    const title = row.querySelector('.title');
    title.textContent = w.title || w.app_id || `#${w.id}`;
    title.title = `${w.app_id}${w.pid ? ' · pid ' + w.pid : ''} · ${w.w}×${w.h} at ${w.x},${w.y}`;
    row.querySelector('.badge').textContent = [w.fullscreen && 'full', w.maximized && 'max', w.minimized && 'min'].filter(Boolean).join(' ');
    const [shot, max, min, close] = row.querySelectorAll('button');
    row.onclick = () => sendControl({ id: w.id, op: 'activate' });
    shot.onclick = e => {
      e.stopPropagation();
      const tab = window.open('', '_blank'); // opened now, inside the click, so popup blockers allow it
      snapshot(w.id, 1).then(b => { tab.location = URL.createObjectURL(b); }).catch(() => tab.close());
    };
    max.onclick = e => { e.stopPropagation(); sendControl({ id: w.id, op: w.maximized ? 'unmaximize' : 'maximize' }); };
    // restoring should also give the window the keyboard, which is what activate does
    min.onclick = e => { e.stopPropagation(); sendControl({ id: w.id, op: w.minimized ? 'activate' : 'minimize' }); };
    close.onclick = e => { e.stopPropagation(); sendControl({ id: w.id, op: 'close' }); };
    return row;
  }));
}
