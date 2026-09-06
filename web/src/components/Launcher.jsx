// The application menu (the installed .desktop launchers, searchable, by category) and the power menu.
import { useEffect, useRef, useState } from 'react';
import { AppWindow, Power, Search } from 'lucide-react';
import { appIcon, applications } from '../api.js';

// freedesktop main categories, as menus label them; the first match names the group
const CATEGORIES = [
  ['AudioVideo', 'Multimedia'], ['Audio', 'Multimedia'], ['Video', 'Multimedia'], ['Development', 'Development'],
  ['Education', 'Education'], ['Game', 'Games'], ['Graphics', 'Graphics'], ['Network', 'Internet'], ['Office', 'Office'],
  ['Science', 'Science'], ['Settings', 'Settings'], ['System', 'System'], ['Utility', 'Accessories'],
];
const groupOf = app => CATEGORIES.find(([c]) => app.categories.includes(c))?.[1] ?? 'Other';

/// A popover under the top bar; a click outside or Escape closes it.
export function Popover({ onClose, className = '', children, ...props }) {
  useEffect(() => {
    const key = e => { if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); onClose(e); } };
    document.addEventListener('keydown', key, true);
    return () => document.removeEventListener('keydown', key, true);
  }, [onClose]);
  return (
    <>
      <div className="absolute inset-x-0 top-11 bottom-0 z-20" onClick={onClose} />
      <div tabIndex={-1} onKeyDown={event => event.stopPropagation()} onKeyUp={event => event.stopPropagation()} {...props} className={`absolute top-12 z-30 flex flex-col rounded-xl border border-zinc-800 bg-zinc-900 shadow-2xl ${className}`}>{children}</div>
    </>
  );
}

export function Launcher({ viewer, onClose }) {
  const [apps, setApps] = useState(null);
  const [q, setQ] = useState('');
  useEffect(() => { applications().then(setApps, () => setApps([])); }, []);
  const needle = q.trim().toLowerCase();
  const shown = (apps ?? []).filter(a => !needle || a.name.toLowerCase().includes(needle) || a.comment?.toLowerCase().includes(needle));
  const groups = needle ? [['Results', shown]] : Object.entries(shown.reduce((g, a) => ((g[groupOf(a)] ??= []).push(a), g), {})).sort(([a], [b]) => a.localeCompare(b));
  const launch = app => { viewer.launch(app.id); onClose(); };
  return (
    <Popover onClose={onClose} className="left-3 max-h-[75vh] w-[26rem] max-w-[calc(100vw-1.5rem)]">
      <label className="flex items-center gap-2 border-b border-zinc-800 px-3 py-2">
        <Search className="size-4 shrink-0 text-zinc-500" />
        <input
          autoFocus
          value={q}
          onChange={e => setQ(e.target.value)}
          onFocus={viewer.releaseInput}
          onKeyDown={e => { if (e.key === 'Enter') e.preventDefault(); }}
          onKeyUp={e => { if (e.key === 'Enter' && needle && shown[0]) launch(shown[0]); }}
          placeholder="Search applications…"
          spellCheck={false}
          autoComplete="off"
          className="min-w-0 flex-1 bg-transparent text-sm text-zinc-100 placeholder:text-zinc-600 focus:outline-none"
        />
      </label>
      <div className="min-h-0 flex-1 overflow-y-auto p-2">
        {apps === null && <div className="px-2 py-6 text-center text-sm text-zinc-600">Loading…</div>}
        {apps !== null && shown.length === 0 && <div className="px-2 py-6 text-center text-sm text-zinc-600">{apps.length ? 'Nothing matches.' : 'No applications found.'}</div>}
        {groups.map(([label, list]) => (
          <section key={label} className="mb-2">
            <h3 className="px-2 pt-1 pb-0.5 text-[10px] font-semibold tracking-wider text-zinc-500 uppercase">{label}</h3>
            {list.map(app => (
              <button key={app.id} type="button" onClick={() => launch(app)}
                onKeyDown={e => { if (e.key === 'Enter') e.preventDefault(); }}
                onKeyUp={e => { if (e.key === 'Enter') launch(app); }} title={app.comment} className="flex w-full items-center gap-3 rounded-md px-2 py-1.5 text-left hover:bg-zinc-800">
                <AppIcon id={app.id} />
                <span className="min-w-0">
                  <span className="block truncate text-sm text-zinc-100">{app.name}</span>
                  {app.comment && <span className="block truncate text-[11px] text-zinc-500">{app.comment}</span>}
                </span>
              </button>
            ))}
          </section>
        ))}
      </div>
    </Popover>
  );
}

function AppIcon({ id }) {
  const [src, setSrc] = useState(null);
  useEffect(() => {
    let live = true;
    appIcon(id).then(u => { if (live) setSrc(u); });
    return () => { live = false; };
  }, [id]);
  return src ? <img src={src} alt="" className="size-8 shrink-0 object-contain" /> : <AppWindow className="size-8 shrink-0 p-1 text-zinc-600" strokeWidth={1.5} />;
}

/// Shut Elsewhere down, after a second click. The keyboard stays on the menu's own buttons
/// (and any key held in the compositor is released), so nothing typed here reaches the desktop.
export function PowerMenu({ viewer, onClose }) {
  const [sure, setSure] = useState(false);
  const ref = useRef(null);
  useEffect(() => { viewer.releaseInput(); ref.current?.focus(); }, [viewer, sure]);
  return (
    <Popover onClose={onClose} className="right-3 w-72 p-3">
      {sure ? (
        <>
          <p className="mb-3 text-sm text-zinc-300">Quit Elsewhere? Every window closes with it, and the desktop is gone until it is started again.</p>
          <div className="flex justify-end gap-2">
            <button ref={ref} type="button" onClick={onClose} className="rounded-md px-3 py-1.5 text-sm text-zinc-300 hover:bg-zinc-800">Cancel</button>
            <button type="button" onClick={event => { viewer.quit(); onClose(event); }} className="rounded-md bg-rose-500 px-3 py-1.5 text-sm font-medium text-white hover:bg-rose-400">Quit</button>
          </div>
        </>
      ) : (
        <button ref={ref} type="button" onClick={() => setSure(true)} className="flex w-full items-center gap-3 rounded-md px-2 py-2 text-left text-sm text-zinc-100 hover:bg-zinc-800">
          <Power className="size-4 text-rose-400" /> Quit Elsewhere
        </button>
      )}
    </Popover>
  );
}
