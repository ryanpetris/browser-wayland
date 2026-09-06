import { TOKEN, WINDOW, PIP } from './api.js';

// Each iframe keeps the existing viewer's handlers, renderer and media in its own document.
export function createPip(viewer) {
  const supported = !PIP && window === window.top && isSecureContext && !!window.documentPictureInPicture?.requestWindow;
  let owned, pending = false, disposed = false, inactive = false;

  function clearContent(entry, restore = true) {
    clearTimeout(entry.timer);
    entry.unsubscribe?.();
    entry.unsubscribe = null;
    entry.parentUnsubscribe?.();
    entry.parentUnsubscribe = null;
    const child = entry.child;
    entry.child = null;
    if (child) {
      child.releaseInput();
      if (restore && entry.desktop) child.handoff(viewer.store.get().sessionId);
      child.dispose();
    }
    entry.frame?.remove();
    entry.frame = null;
    if (entry.desktop && restore) viewer.setPlaybackEnabled(true);
  }

  function close(restore = true) {
    const entry = owned;
    if (!entry) return;
    owned = null;
    clearContent(entry, restore);
    entry.win.close();
  }

  async function open(id = WINDOW) {
    if (!supported || disposed || inactive || pending) return;
    const target = id == null ? null : String(id);
    if (owned && !owned.win.closed && owned.target === target) { owned.win.focus(); return; }
    pending = true;
    try {
      // requestWindow must run in the original click, before any await.
      const win = owned?.win && !owned.win.closed ? owned.win : await documentPictureInPicture.requestWindow({ width: 640, height: 480 });
      if (disposed || inactive) { win.close(); return; }
      if (owned) { const previous = owned; owned = null; clearContent(previous); }
      const entry = owned = { win, target, desktop: target === null };
      win.onpagehide = () => { if (owned === entry) close(); };
      // The child's compact-mode detection reads this before its module evaluates.
      win.bwReturn = () => { window.focus(); close(); };
      win.document.title = 'browser-wayland';
      win.document.body.style.cssText = 'margin:0;height:100vh;background:#09090b';
      const frame = entry.frame = win.document.createElement('iframe');
      frame.title = target === null ? 'Remote desktop' : `Remote window ${target}`;
      frame.style.cssText = 'width:100%;height:100%;border:0;display:block';
      frame.allow = 'autoplay; clipboard-read; clipboard-write';
      const url = new URL(location.pathname, location.origin);
      url.searchParams.set('pip', '1');
      if (target !== null) url.searchParams.set('window', target);
      url.hash = new URLSearchParams({ token: TOKEN }).toString();
      frame.src = url.href;
      if (entry.desktop) viewer.setPlaybackEnabled(false);
      frame.onload = () => {
        if (owned !== entry) return;
        const child = entry.child = frame.contentWindow.bw;
        if (!child) return; // the connection deadline also covers an initial about:blank load
        let handedTo;
        const update = () => {
          if (owned !== entry) return;
          const state = child.store.get();
          const title = state.windowTitle || (entry.desktop ? 'Remote desktop' : `Window ${target}`);
          if (win.document.title !== title) win.document.title = title;
          if (['unauthorized', 'no-token', 'gone', 'closed', 'quit'].includes(state.status)) { close(); return; }
          if (state.status !== 'connected') return;
          clearTimeout(entry.timer);
          if (entry.desktop) {
            child.setPlaybackEnabled(true);
            if (state.sessionId != null && handedTo !== state.sessionId && viewer.store.get().role === 'controller') {
              handedTo = state.sessionId;
              if (viewer.store.get().mic || viewer.store.get().cam) child.notice('Capture stops when control moves. Return to the main viewer to restart the microphone or camera.');
              viewer.releaseInput();
              viewer.handoff(state.sessionId);
            }
          }
        };
        entry.unsubscribe = child.store.subscribe(update);
        if (entry.desktop) entry.parentUnsubscribe = viewer.store.subscribe(update);
        frame.contentWindow.addEventListener('pagehide', () => { if (owned === entry) close(); }, { once: true });
        update();
      };
      entry.timer = setTimeout(() => { if (owned === entry) { viewer.notice('Picture-in-picture viewer did not connect.'); close(); } }, 15000);
      win.document.body.replaceChildren(frame);
      win.focus();
    } catch {
      viewer.notice('Picture-in-picture could not open. Try again from the viewer button.');
      close();
    } finally { pending = false; }
  }

  const hide = () => { inactive = true; close(false); };
  const show = () => { if (!inactive) return; inactive = false; viewer.setPlaybackEnabled(true); };
  const dispose = () => {
    disposed = true; close(false);
    window.removeEventListener('pagehide', hide);
    window.removeEventListener('pageshow', show);
  };
  if (supported) {
    window.addEventListener('pagehide', hide);
    window.addEventListener('pageshow', show);
  }
  return { supported, open, close, closeDesktop: () => { if (owned?.desktop) close(); }, dispose };
}
