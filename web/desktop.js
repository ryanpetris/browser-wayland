// Desktop UI on top of the video: the window list, click-to-activate, colour-coded borders, and the
// UI elements of the focused window (from /api/windows/{id}/elements) drawn over it.
// Fed by the WINDOWS message; talks back with CONTROL (see crates/bw-server/src/protocol.rs).

let windows = [];
let sendControl = () => {}, streamSize = () => null, releaseInput = () => {}; // streamSize(): logical {w, h, scale} of the video, or null before Config
const rows = new Map(); // window id -> its panel row, kept across list updates so thumbnails only reload when their URL changes
let thumbQueue = Promise.resolve(); // the server renders one snapshot at a time (429 otherwise), so fetch them one by one
let bordersOn = false, elementsOn = false;
let elements = null, elementsKey = '', elementsTimer = 0; // the focused window's elements: {id, status, page}
const panel = document.getElementById('panel'), wins = document.getElementById('wins'), spawn = document.getElementById('spawn');
const overlay = document.getElementById('overlay'), canvas = document.getElementById('c');

export const getWindows = () => windows;
export const control = obj => sendControl(obj);
const TOKEN = new URLSearchParams(location.search).get('token') ?? '';
/// fetch() with the bearer token, for the HTTP API.
export const api = (path, init = {}) => fetch(path, { ...init, headers: { ...init.headers, Authorization: `Bearer ${TOKEN}` } });
export const snapshotUrl = (id, scale = 1) => `${id == null ? '/api/screenshot.png' : `/api/windows/${id}/snapshot.png`}?scale=${scale}`;
export const snapshot = async (id, scale = 1) => (await api(snapshotUrl(id, scale))).blob();
export const elementsOf = async id => (await api(`/api/windows/${id}/elements`)).json();

export function initDesktop(send, size, release) {
  sendControl = send;
  streamSize = size;
  releaseInput = release;
  spawn.onfocus = () => releaseInput(); // a key held on the canvas must not stay held in the compositor
  document.getElementById('panelbtn').onclick = () => { panel.classList.toggle('open'); renderList(); };
  try { bordersOn = localStorage.getItem('bw.borders') === '1'; elementsOn = localStorage.getItem('bw.elements') === '1'; } catch {}
  document.getElementById('borders').onclick = () => {
    bordersOn = !bordersOn;
    try { localStorage.setItem('bw.borders', bordersOn ? '1' : '0'); } catch {}
    renderBorders();
  };
  document.getElementById('elements').onclick = () => {
    elementsOn = !elementsOn;
    try { localStorage.setItem('bw.elements', elementsOn ? '1' : '0'); } catch {}
    fetchElements();
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
  fetchElements();
  renderBorders();
}

const focusedWindow = () => windows.find(w => w.focused && !w.minimized);

// The focused window's elements, fetched again when anything the answer depends on changed: the focus,
// the title, the content (updated_ms, whole seconds), the geometry or the stream scale (Chromium's web
// content is scaled by it). The 300 ms delay merges a burst of list updates into one request.
export function fetchElements() {
  const f = focusedWindow();
  const key = elementsOn && f ? `${f.id}/${f.title}/${f.updated_ms}/${f.w}x${f.h}+${f.geo_x}+${f.geo_y}@${streamSize()?.scale}` : '';
  if (key === elementsKey) return;
  elementsKey = key;
  clearTimeout(elementsTimer);
  if (!key) { elements = null; return; }
  elementsTimer = setTimeout(async () => {
    const res = await api(`/api/windows/${f.id}/elements`).catch(() => null);
    if (elementsKey !== key) return; // superseded while in flight; the newer request is on its way
    if (!res) { elementsKey = ''; return; } // network failure: the next list update retries
    elements = { id: f.id, status: res.status, page: await res.json().catch(() => ({})) };
    renderBorders();
  }, 300);
}

// One rectangle per visible window (the same hue as the list) and one per element of the focused
// window, in CSS px over the canvas.
export function renderBorders() {
  overlay.hidden = !bordersOn && !elementsOn;
  if (overlay.hidden) return;
  const size = streamSize();
  if (!size) return;
  const r = canvas.getBoundingClientRect();
  const sx = r.width / size.w, sy = r.height / size.h;
  const box = (d, x, y, w, h) => { d.style.cssText = `left:${r.left + x * sx}px;top:${r.top + y * sy}px;width:${w * sx}px;height:${h * sy}px;`; return d; };
  const nodes = [];
  if (bordersOn) for (const w of windows.filter(w => !w.minimized)) {
    const d = box(document.createElement('div'), w.x, w.y, w.w, w.h);
    d.className = w.focused ? 'focused' : '';
    d.style.borderColor = color(w);
    const label = document.createElement('span');
    label.textContent = w.app_id || w.title;
    label.style.background = color(w);
    d.append(label);
    nodes.push(d);
  }
  const f = focusedWindow();
  if (elementsOn && f && elements?.id === f.id) {
    const { status, page } = elements;
    for (const e of page.elements ?? []) {
      const d = box(document.createElement('div'), f.x + e.x, f.y + e.y, e.w, e.h);
      d.className = 'el';
      d.style.borderColor = `hsl(${hue(e.role)} 80% 50%)`;
      nodes.push(d);
    }
    const why = status !== 200 ? (page.error || `HTTP ${status}`) : page.level !== 'full' && `no elements: ${page.level}${page.toolkit ? ' (' + page.toolkit + ')' : ''}`;
    if (why) {
      const d = box(document.createElement('div'), f.x, f.y + f.h, f.w, 0);
      d.className = 'note';
      d.textContent = why;
      nodes.push(d);
    }
  }
  overlay.replaceChildren(...nodes);
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
