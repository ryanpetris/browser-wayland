// The display: the video canvas, sized by its container (the desktop's output takes that size), with
// the overlays and status banners on top. Fullscreen is requested on this element, so the chrome goes away.
import { useEffect, useRef, useState } from 'react';
import { Loader2, MonitorX, RefreshCw } from 'lucide-react';
import { useStore } from '../store.js';
import { hue, windowColor } from './ui.jsx';

export function Stage({ ref, viewer, windowMode, borders, elements }) {
  const el = useRef(null);
  const [size, setSize] = useState({ w: 0, h: 0 });
  useEffect(() => {
    const ro = new ResizeObserver(([e]) => {
      const { width, height } = e.contentRect;
      setSize({ w: width, h: height });
      viewer.setStage(width, height);
    });
    ro.observe(el.current);
    return () => ro.disconnect();
  }, [viewer]);
  return (
    <div
      ref={node => { el.current = node; if (typeof ref === 'function') ref(node); else if (ref) ref.current = node; }}
      className="relative flex min-w-0 flex-1 items-center justify-center overflow-hidden bg-black"
    >
      <canvas ref={viewer.attach} tabIndex={-1} className={`stage block outline-none ${windowMode ? '' : 'h-full w-full'}`} />
      {(borders || elements) && <Overlay viewer={viewer} size={size} borders={borders} elements={elements} />}
      <Banner viewer={viewer} />
    </div>
  );
}

// One rectangle per visible window (the same hue as its row) and one per element of the focused
// window, in CSS px over the canvas, which fills the stage.
function Overlay({ viewer, size, borders, elements }) {
  const windows = useStore(viewer.store, s => s.windows);
  const stream = useStore(viewer.store, s => s.stream);
  const els = useStore(viewer.store, s => s.elements);
  if (!stream || !size.w) return null;
  const sx = size.w / (stream.width / stream.scale), sy = size.h / (stream.height / stream.scale);
  const box = (x, y, w, h) => ({ left: x * sx, top: y * sy, width: w * sx, height: h * sy });
  const f = windows.find(w => w.focused && !w.minimized);
  const page = elements && f && els?.id === f.id ? els : null;
  const why = page && (page.status !== 200 ? page.page.error || `HTTP ${page.status}` : page.page.level !== 'full' && `no elements: ${page.page.level}${page.page.toolkit ? ` (${page.page.toolkit})` : ''}`);
  return (
    <div className="pointer-events-none absolute inset-0 overflow-hidden">
      {borders && windows.filter(w => !w.minimized).map(w => (
        <div key={w.id} className={`absolute box-border ${w.focused ? 'border-[3px]' : 'border-2'}`} style={{ ...box(w.x, w.y, w.w, w.h), borderColor: windowColor(w) }}>
          <span className="absolute -top-0.5 -left-0.5 rounded-br px-1 font-mono text-[11px] leading-4 text-zinc-950" style={{ background: windowColor(w) }}>
            {w.app_id || w.title}
          </span>
        </div>
      ))}
      {page && (page.page.elements ?? []).map((e, i) => (
        <div key={i} className="absolute box-border border" style={{ ...box(f.x + e.x, f.y + e.y, e.w, e.h), borderColor: `hsl(${hue(e.role)} 80% 55%)` }} />
      ))}
      {why && (
        <div className="absolute rounded-b bg-zinc-950/80 px-1.5 font-mono text-[11px] leading-5 whitespace-nowrap text-zinc-200" style={{ left: f.x * sx, top: (f.y + f.h) * sy }}>
          {why}
        </div>
      )}
    </div>
  );
}

// What the connection is doing, when it isn't simply showing the desktop.
function Banner({ viewer }) {
  const status = useStore(viewer.store, s => s.status);
  const reason = useStore(viewer.store, s => s.reason);
  const stream = useStore(viewer.store, s => s.stream);
  if (status === 'connected' || status === 'no-token' || status === 'unauthorized') return null;
  if (status === 'retrying' || (status === 'connecting' && stream)) {
    return (
      <div className="absolute top-3 left-1/2 flex -translate-x-1/2 items-center gap-2 rounded-full border border-zinc-700 bg-zinc-900/90 px-3 py-1 text-xs text-zinc-300 shadow-lg">
        <Loader2 className="size-3.5 animate-spin text-amber-400" /> Reconnecting…
      </div>
    );
  }
  const card = 'flex flex-col items-center gap-3 rounded-xl border border-zinc-800 bg-zinc-900/95 px-8 py-6 text-center shadow-2xl';
  if (status === 'connecting') {
    return (
      <div className={`absolute ${card}`}>
        <Loader2 className="size-6 animate-spin text-indigo-400" />
        <div className="text-sm text-zinc-300">Connecting…</div>
      </div>
    );
  }
  if (status === 'replaced') {
    return (
      <div className={`absolute ${card}`}>
        <MonitorX className="size-6 text-rose-400" />
        <div className="text-sm text-zinc-200">Another viewer took over</div>
        <div className="text-xs text-zinc-500">Only one viewer at a time; the newest wins.</div>
        <button type="button" onClick={viewer.reconnect} className="mt-1 inline-flex items-center gap-1.5 rounded-md bg-indigo-500 px-3 py-1.5 text-sm font-medium text-white hover:bg-indigo-400">
          <RefreshCw className="size-3.5" /> Take over
        </button>
      </div>
    );
  }
  return (
    <div className={`absolute ${card}`}>
      <MonitorX className="size-6 text-zinc-500" />
      <div className="text-sm text-zinc-200">{reason || 'Window closed'}</div>
      <div className="text-xs text-zinc-500">This tab showed one window; it is gone.</div>
    </div>
  );
}
