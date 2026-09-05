// The token and the HTTP API. The token arrives once in the URL fragment (`#token=`, which never
// reaches the server or a proxy log; `?token=` is accepted too), then lives in sessionStorage (this
// tab only, copied into popups this page opens) and leaves the address bar.
export const TOKEN = (() => {
  const url = new URL(location);
  const t = new URLSearchParams(url.hash.slice(1)).get('token') || url.searchParams.get('token');
  if (!t) {
    try { return sessionStorage.getItem('bw.token') ?? ''; } catch { return ''; }
  }
  try { sessionStorage.setItem('bw.token', t); } catch {}
  url.hash = '';
  url.searchParams.delete('token');
  try { history.replaceState(null, '', url); } catch {}
  return t;
})();

/// `?window=ID`: this tab shows one application window as its own stream.
export const WINDOW = new URLSearchParams(location.search).get('window');

/// fetch() with the bearer token.
export const api = (path, init = {}) => fetch(path, { ...init, headers: { ...init.headers, Authorization: `Bearer ${TOKEN}` } });
export const snapshotUrl = (id, scale = 1) => `${id == null ? '/api/screenshot.png' : `/api/windows/${id}/snapshot.png`}?scale=${scale}`;
export const snapshot = async (id, scale = 1) => (await api(snapshotUrl(id, scale))).blob();
export const elementsOf = async id => (await api(`/api/windows/${id}/elements`)).json();
export const applications = async () => (await api('/api/applications')).json();
export const control = body => api('/api/control', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });

// An application's icon as a blob URL (<img> can't send the bearer header), fetched once per page; null when it has none.
const icons = new Map();
export const appIcon = id => {
  if (!icons.has(id)) icons.set(id, api(`/api/applications/${encodeURIComponent(id)}/icon`).then(r => (r.ok ? r.blob().then(URL.createObjectURL) : null)).catch(() => null));
  return icons.get(id);
};

// A window's icon as a blob URL, cached by what decides it (window, the client's icon name, app id).
const windowIcons = new Map();
export const windowIcon = (id, key) => {
  if (!windowIcons.has(key)) windowIcons.set(key, api(`/api/windows/${id}/icon`).then(r => (r.ok ? r.blob().then(URL.createObjectURL) : null)).catch(() => null));
  return windowIcons.get(key);
};

// The transfer folder: list, upload (the final name comes back), download through a blob (no token in URLs), delete.
const ok = r => (r.ok ? r : Promise.reject(new Error(`HTTP ${r.status}`)));
export const files = async () => ok(await api('/api/files')).json();
export const uploadFile = async file => ok(await api(`/api/files/${encodeURIComponent(file.name)}`, { method: 'PUT', body: file })).json();
export const deleteFile = async name => ok(await api(`/api/files/${encodeURIComponent(name)}`, { method: 'DELETE' }));
// Saved under the name the server serves it as (a clipboard file may have changed since its name was shown).
const download = async (path, name) => {
  const r = ok(await api(path));
  const served = /filename\*=UTF-8''([^;]+)/.exec(r.headers.get('content-disposition') ?? '');
  const a = Object.assign(document.createElement('a'), { href: URL.createObjectURL(await r.blob()), download: served ? decodeURIComponent(served[1]) : name });
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 60000);
};
export const downloadFile = name => download(`/api/files/${encodeURIComponent(name)}`, name);

// Files on the clipboard: put files of the transfer folder there; fetch the i-th file copied in the desktop.
export const clipboardFiles = names => api('/api/clipboard/files', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ names }) });
export const downloadClipboardFile = (index, name) => download(`/api/clipboard/files/${index}`, name);

// A notification's icon as a blob URL (the caller revokes it), or null.
export const notificationIcon = id => api(`/api/notifications/${id}/icon`).then(r => (r.ok ? r.blob().then(URL.createObjectURL) : null)).catch(() => null);

// The server renders one snapshot at a time (429 otherwise): thumbnails are fetched one by one, and
// one nobody wants any more by its turn (`wanted()` false) is skipped.
let queue = Promise.resolve();
export const queuedSnapshot = (id, scale, wanted = () => true) => {
  const run = () => (wanted() ? snapshot(id, scale) : null);
  return (queue = queue.then(run, run));
};

/// A remembered UI preference.
export const pref = {
  get: (key, fallback) => { try { const v = localStorage.getItem(`bw.${key}`); return v === null ? fallback : v === '1'; } catch { return fallback; } },
  set: (key, on) => { try { localStorage.setItem(`bw.${key}`, on ? '1' : '0'); } catch {} },
  getStr: (key, fallback) => { try { return localStorage.getItem(`bw.${key}`) ?? fallback; } catch { return fallback; } },
  setStr: (key, v) => { try { localStorage.setItem(`bw.${key}`, v); } catch {} },
};
export const codecs = async () => (await api('/api/codecs')).json();
