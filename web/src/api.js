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
};
