// The token and the HTTP API. The token arrives once in the URL fragment (`#token=`, which never
// reaches the server or a proxy log), then lives in sessionStorage (this
// tab only, copied into popups this page opens) and leaves the address bar.
export const TOKEN = (() => {
  const url = new URL(location);
  const t = new URLSearchParams(url.hash.slice(1)).get('token');
  if (t) {
    try { sessionStorage.setItem('elsewhere.token', t); } catch {}
    url.hash = '';
  }
  url.searchParams.delete('token');
  try { history.replaceState(null, '', url); } catch {}
  if (t) return t;
  try { return sessionStorage.getItem('elsewhere.token') ?? ''; } catch { return ''; }
})();

/// `?window=ID`: this tab shows one application window as its own stream.
export const WINDOW = new URLSearchParams(location.search).get('window');

// Compact viewer hosted in our same-origin Document PiP iframe.
export const PIP = (() => {
  try { return window !== window.parent && typeof window.parent.elsewhereReturn === 'function' && new URLSearchParams(location.search).has('pip'); }
  catch { return false; }
})();

/// fetch() with the bearer token.
export const api = (path, init = {}) => fetch(path, { ...init, headers: { ...init.headers, Authorization: `Bearer ${TOKEN}` } });
export const snapshotUrl = (id, sizing = {}) => `${id == null ? '/api/screenshot.png' : `/api/windows/${id}/snapshot.png`}?${new URLSearchParams(sizing)}`;
export const snapshot = async (id, sizing = {}, signal) => {
  const response = await api(snapshotUrl(id, sizing), { signal });
  if (!response.ok) throw new Error(await response.text());
  return response.blob();
};
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

// File requests carry this client's directory; bearer tokens stay out of URLs.
const ok = r => (r.ok ? r : Promise.reject(new Error(`HTTP ${r.status}`)));
const fileResult = async r => {
  if (r.ok) return r.status === 204 ? null : r.json();
  const body = await r.json().catch(() => ({}));
  throw Object.assign(new Error(body.error || `HTTP ${r.status}`), { code: body.code, status: r.status });
};
const fileUrl = (name, path) => `/api/files/${encodeURIComponent(name)}?${new URLSearchParams({ path })}`;
export const files = async (query, signal) => fileResult(await api(`/api/files?${new URLSearchParams(query)}`, { signal }));
export const uploadFile = async (file, batch, path, signal) => fileResult(await api(batch ? `/api/drop/${batch}/${encodeURIComponent(file.name)}` : fileUrl(file.name, path), { method: 'PUT', body: file, signal }));
export const deleteFile = async (name, path) => fileResult(await api(fileUrl(name, path), { method: 'DELETE' }));
export const manageFile = async action => fileResult(await api('/api/files', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(action) }));
// Saved under the name the server serves it as (a clipboard file may have changed since its name was shown).
const download = async (path, name) => {
  const r = ok(await api(path));
  const served = /filename\*=UTF-8''([^;]+)/.exec(r.headers.get('content-disposition') ?? '');
  const a = Object.assign(document.createElement('a'), { href: URL.createObjectURL(await r.blob()), download: served ? decodeURIComponent(served[1]) : name });
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 60000);
};
export const downloadFile = (name, path) => download(fileUrl(name, path), name);

// Files on the clipboard: put files of the transfer folder, or of a staged batch, there; fetch the i-th file copied in the desktop.
export const clipboardFiles = (names, batch) => api('/api/clipboard/files', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ names, batch }) });
export const downloadClipboardFile = (index, name) => download(`/api/clipboard/files/${index}`, name);

// A notification's icon as a blob URL (the caller revokes it), or null.
export const notificationIcon = id => api(`/api/notifications/${id}/icon`).then(r => (r.ok ? r.blob().then(URL.createObjectURL) : null)).catch(() => null);

// Thumbnail jobs recheck eligibility when they reach this serialized queue.
let queue = Promise.resolve();
export const queueSnapshot = run => (queue = queue.then(run, run));

/// A remembered UI preference.
export const pref = {
  get: (key, fallback) => { try { const v = localStorage.getItem(`elsewhere.${key}`); return v === null ? fallback : v === '1'; } catch { return fallback; } },
  set: (key, on) => { try { localStorage.setItem(`elsewhere.${key}`, on ? '1' : '0'); } catch {} },
  getStr: (key, fallback) => { try { return localStorage.getItem(`elsewhere.${key}`) ?? fallback; } catch { return fallback; } },
  setStr: (key, v) => { try { localStorage.setItem(`elsewhere.${key}`, v); } catch {} },
};
export const codecs = async () => (await api('/api/codecs')).json();
