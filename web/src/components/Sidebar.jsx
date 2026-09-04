// The side panel: the window list (with actions and a command box) or the statistics.
import { useEffect, useRef, useState } from 'react';
import { Camera, ChevronDown, ChevronUp, ExternalLink, Maximize2, Minimize2, Play, X } from 'lucide-react';
import { useStore } from '../store.js';
import { queuedSnapshot, snapshot } from '../api.js';
import { codecName, windowColor } from './ui.jsx';

// The two panels stay mounted (hidden) so the window list keeps its thumbnails across toggles.
export function Sidebar({ viewer, tab, onTab, hidden }) {
  return (
    <aside hidden={hidden} className="flex w-80 shrink-0 flex-col border-l border-zinc-800 bg-zinc-900">
      <nav className="flex shrink-0 border-b border-zinc-800 text-sm">
        {[['windows', 'Windows'], ['stats', 'Statistics']].map(([t, label]) => (
          <button
            key={t}
            type="button"
            onClick={e => { onTab(t); e.currentTarget.blur(); }}
            className={`-mb-px flex-1 border-b-2 px-3 py-2 transition-colors ${tab === t ? 'border-indigo-400 text-zinc-100' : 'border-transparent text-zinc-500 hover:text-zinc-300'}`}
          >
            {label}
          </button>
        ))}
      </nav>
      <div hidden={tab !== 'windows'} className="min-h-0 flex-1 overflow-y-auto"><WindowList viewer={viewer} /></div>
      <div hidden={tab !== 'stats'} className="min-h-0 flex-1 overflow-y-auto"><StatsPanel viewer={viewer} /></div>
    </aside>
  );
}

function WindowList({ viewer }) {
  const windows = useStore(viewer.store, s => s.windows);
  const order = windows.slice().sort((a, b) => a.minimized - b.minimized || b.z - a.z); // top-most first, minimized last
  return (
    <div className="flex flex-col">
      <Spawn viewer={viewer} />
      {order.length === 0 && <div className="px-4 py-8 text-center text-sm text-zinc-600">No windows yet. Run a command above.</div>}
      {order.map(w => <WindowRow key={w.id} viewer={viewer} w={w} />)}
    </div>
  );
}

// Starts a program on the desktop. While it has the keyboard, keys stay in the page (viewer.js skips text fields).
function Spawn({ viewer }) {
  const [cmd, setCmd] = useState('');
  const run = () => { if (cmd.trim()) { viewer.spawn(cmd.trim()); setCmd(''); } };
  return (
    <div className="flex gap-1.5 border-b border-zinc-800 p-2">
      <input
        value={cmd}
        onChange={e => setCmd(e.target.value)}
        onFocus={viewer.releaseInput}
        onKeyDown={e => { if (e.key === 'Enter') run(); if (e.key === 'Escape') e.currentTarget.blur(); }}
        placeholder="Run a command…"
        spellCheck={false}
        autoComplete="off"
        className="min-w-0 flex-1 rounded-md border border-zinc-700 bg-zinc-950 px-2.5 py-1.5 font-mono text-xs text-zinc-200 placeholder:text-zinc-600 focus:border-indigo-400 focus:outline-none"
      />
      <button type="button" onClick={e => { run(); e.currentTarget.blur(); }} title="Run" className="inline-flex size-8 items-center justify-center rounded-md bg-indigo-500 text-white hover:bg-indigo-400">
        <Play className="size-3.5" />
      </button>
    </div>
  );
}

function WindowRow({ viewer, w }) {
  const badges = [w.fullscreen && 'fullscreen', w.maximized && 'maximized', w.minimized && 'minimized'].filter(Boolean);
  const act = (op, e) => { e.stopPropagation(); e.currentTarget.blur(); viewer.control({ id: w.id, op }); };
  return (
    <div
      onClick={() => viewer.activate(w.id)}
      className={`group relative flex cursor-pointer items-center gap-2.5 border-b border-zinc-800/70 px-2.5 py-2 transition-colors hover:bg-zinc-800/60 ${w.focused ? 'bg-indigo-500/10' : ''} ${w.minimized ? 'opacity-60' : ''}`}
    >
      <Thumb id={w.id} updated={w.updated_ms} />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-1.5">
          <span className="size-2 shrink-0 rounded-full" style={{ background: windowColor(w) }} />
          <span className={`truncate text-sm ${w.focused ? 'text-zinc-100' : 'text-zinc-300'}`} title={w.title}>{w.title || w.app_id || `#${w.id}`}</span>
        </div>
        <div className="mt-0.5 flex items-center gap-1.5 text-[11px] text-zinc-500">
          <span className="truncate" title={w.app_id}>{w.app_id || (w.x11 ? 'X11' : 'Wayland')}</span>
          <span className="shrink-0 font-mono" title={w.decoration ? 'plus the title bar' : ''}>{w.w}×{w.h}</span>
          {badges.map(b => <span key={b} className="shrink-0 rounded bg-zinc-800 px-1 text-[10px] text-zinc-400">{b}</span>)}
        </div>
      </div>
      {/* the actions float over the row's end on hover, so titles keep the width */}
      <div className="absolute inset-y-1.5 right-2 flex items-center gap-px rounded-md border border-zinc-700 bg-zinc-800 px-0.5 opacity-0 shadow-md transition-opacity group-hover:opacity-100 focus-within:opacity-100">
        <Action icon={ExternalLink} label="Open in its own window" onClick={e => {
          e.stopPropagation(); e.currentTarget.blur();
          // the window's size plus the popup's own bars (TopBar h-11 + StatusBar h-7), so it shows 1:1
          window.open(`/?window=${w.id}`, `bw-window-${w.id}`, `popup,width=${w.w},height=${w.h + 72}`);
        }} />
        <Action icon={Camera} label="Snapshot (PNG)" onClick={e => {
          e.stopPropagation(); e.currentTarget.blur();
          const tab = window.open('', '_blank'); // opened now, inside the click, so popup blockers allow it
          snapshot(w.id, 1).then(b => { tab.location = URL.createObjectURL(b); }).catch(() => tab.close());
        }} />
        <Action icon={w.maximized ? Minimize2 : Maximize2} label={w.maximized ? 'Restore' : 'Maximize'} onClick={e => act(w.maximized ? 'unmaximize' : 'maximize', e)} />
        <Action icon={w.minimized ? ChevronUp : ChevronDown} label={w.minimized ? 'Restore' : 'Minimize'} onClick={e => act(w.minimized ? 'activate' : 'minimize', e)} />
        <Action icon={X} label="Close" onClick={e => act('close', e)} className="hover:text-rose-300" />
      </div>
    </div>
  );
}

function Action({ icon: Icon, label, onClick, className = '' }) {
  return (
    <button type="button" title={label} aria-label={label} onClick={onClick} className={`inline-flex size-6 items-center justify-center rounded text-zinc-400 hover:bg-zinc-700 hover:text-zinc-100 ${className}`}>
      <Icon className="size-3.5" strokeWidth={1.75} />
    </button>
  );
}

// A window's thumbnail, refetched when its content changed (updated_ms has whole-second resolution, so a
// busy window costs at most one render per second). <img> can't send the bearer header, so the PNG comes
// through fetch() and a blob URL; the old picture stays until the new one is in.
function Thumb({ id, updated }) {
  const [src, setSrc] = useState('');
  const url = useRef('');
  useEffect(() => {
    let live = true;
    queuedSnapshot(id, 0.12, () => live).then(b => {
      if (!live || !b) return;
      if (url.current) URL.revokeObjectURL(url.current);
      url.current = URL.createObjectURL(b);
      setSrc(url.current);
    }).catch(() => {});
    return () => { live = false; };
  }, [id, updated]);
  useEffect(() => () => { if (url.current) URL.revokeObjectURL(url.current); }, []);
  return (
    <div className="h-10 w-16 shrink-0 overflow-hidden rounded bg-black/60 ring-1 ring-zinc-800">
      {src && <img src={src} alt="" className="h-full w-full object-contain" />}
    </div>
  );
}

const ms = v => (v == null ? '–' : v.toFixed(1));

function StatsPanel({ viewer }) {
  const s = useStore(viewer.store, st => st.stats);
  const stream = useStore(viewer.store, st => st.stream);
  const renderer = useStore(viewer.store, st => st.renderer);
  const locked = useStore(viewer.store, st => st.locked);
  const t = s.timings;
  return (
    <div className="flex flex-col gap-4 p-3 text-xs">
      <Section title="Stream">
        <Row label="Codec" value={stream ? `${codecName(stream.codec)} ${stream.codec}` : '–'} />
        <Row label="Size" value={stream ? `${stream.width}×${stream.height} @${stream.scale.toFixed(2)}` : '–'} />
        <Row label="Renderer" value={renderer} />
        <Row label="Frame rate" value={`${s.fps} fps`} />
        <Row label="Bandwidth" value={`${s.mbps.toFixed(1)} Mbit/s`} />
        <Row label="Input → paint" value={`${s.latencyMs.toFixed(0)} ms`} />
      </Section>
      <Section title="Timings, last second (p50 / p95)">
        <Row label="Received → decoded" value={t ? `${ms(t.decode[0])} / ${ms(t.decode[1])} ms` : '–'} />
        <Row label="Decoded → painted" value={t ? `${ms(t.paint[0])} / ${ms(t.paint[1])} ms` : '–'} />
        <Row label="Paint interval" value={t ? `${ms(t.interval[0])} / ${ms(t.interval[1])} ms` : '–'} />
        <Row label="Decode queue" value={s.queue} />
      </Section>
      <Section title="Frames">
        <Row label="Painted / received" value={`${s.frames} / ${s.received}`} />
        <Row label="Keyframes" value={`${s.keyframes} (${s.sinceKey} since last)`} />
        <Row label="Lost / dropped / errors" value={`${s.lost} / ${s.dropped} / ${s.decodeErrors}`} warn={s.lost + s.dropped + s.decodeErrors > 0} />
      </Section>
      <Section title="Audio">
        {s.audio ? (
          <>
            <Row label="State" value={s.audio.state} />
            <Row label="Packets / decoded" value={`${s.audio.packets} / ${s.audio.decoded}`} />
            <Row label="Lead" value={`${s.audio.lead.toFixed(0)} ms`} />
            <Row label="Underruns" value={s.underruns} warn={s.underruns > 0} />
            <div className="mt-1 h-1 overflow-hidden rounded bg-zinc-800"><div className="h-full bg-emerald-400 transition-[width]" style={{ width: `${(s.audio.level / 255) * 100}%` }} /></div>
          </>
        ) : <Row label="State" value="off" />}
      </Section>
      <Section title="Connection">
        <Row label="Connects / closes" value={`${s.connects} / ${s.closes.length}`} />
        {s.closes.length > 0 && <Row label="Last close" value={s.closes[s.closes.length - 1]} />}
        <Row label="Pointer lock" value={`${locked ? 'locked' : 'free'} (${s.lockRequests} requests${s.lockError ? ', ' + s.lockError : ''})`} />
      </Section>
    </div>
  );
}

function Section({ title, children }) {
  return (
    <section>
      <h3 className="mb-1.5 text-[10px] font-semibold tracking-wider text-zinc-500 uppercase">{title}</h3>
      <div className="flex flex-col gap-1">{children}</div>
    </section>
  );
}

function Row({ label, value, warn = false }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <span className="text-zinc-500">{label}</span>
      <span className={`truncate font-mono ${warn ? 'text-amber-300' : 'text-zinc-200'}`}>{value}</span>
    </div>
  );
}
