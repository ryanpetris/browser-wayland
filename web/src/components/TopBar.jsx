import { Expand, LayoutList, Maximize, PanelRight, ScanSearch } from 'lucide-react';
import { useStore } from '../store.js';
import { IconButton } from './ui.jsx';

const CODEC = { avc1: 'H.264', hev1: 'HEVC', hvc1: 'HEVC', vp09: 'VP9' };
const codecName = c => CODEC[c?.split('.')[0]] ?? c;

const STATUS = {
  'no-token': ['bg-zinc-500', 'No token'],
  connecting: ['bg-amber-400 animate-pulse', 'Connecting…'],
  connected: ['bg-emerald-400', 'Connected'],
  retrying: ['bg-amber-400 animate-pulse', 'Reconnecting…'],
  replaced: ['bg-rose-400', 'Replaced by another viewer'],
  unauthorized: ['bg-rose-400', 'Not authorized'],
  gone: ['bg-zinc-500', 'Window closed'],
};

export function TopBar({ viewer, windowMode, borders, onBorders, elements, onElements, sidebar, onSidebar, onFullscreen }) {
  const status = useStore(viewer.store, s => s.status);
  const stream = useStore(viewer.store, s => s.stream);
  const windowTitle = useStore(viewer.store, s => s.windowTitle);
  const [dot, text] = STATUS[status];
  return (
    <header className="flex h-11 shrink-0 items-center gap-3 border-b border-zinc-800 bg-zinc-900 px-3">
      <div className="flex items-center gap-2 font-medium text-zinc-100">
        <Logo />
        <span className="truncate">{windowMode ? windowTitle || `Window ${new URLSearchParams(location.search).get('window')}` : 'browser-wayland'}</span>
      </div>
      <div className="flex min-w-0 items-center gap-2 text-xs text-zinc-400">
        <span className={`size-2 shrink-0 rounded-full ${dot}`} />
        <span className="truncate">{text}</span>
        {stream && status === 'connected' && (
          <span className="hidden truncate font-mono text-zinc-500 sm:inline">
            · {codecName(stream.codec)} {stream.width}×{stream.height}{stream.scale !== 1 ? ` @${stream.scale.toFixed(2)}` : ''}
          </span>
        )}
      </div>
      <div className="ml-auto flex items-center gap-1">
        {!windowMode && (
          <>
            <IconButton icon={Maximize} label="Window borders" active={borders} onClick={onBorders} />
            <IconButton icon={ScanSearch} label="UI elements of the focused window" active={elements} onClick={onElements} />
            <IconButton icon={sidebar ? PanelRight : LayoutList} label="Windows and statistics" active={sidebar} onClick={onSidebar} />
            <span className="mx-1 h-5 w-px bg-zinc-800" />
          </>
        )}
        <IconButton icon={Expand} label="Fullscreen (browser shortcuts go to the desktop)" onClick={onFullscreen} />
      </div>
    </header>
  );
}

function Logo() {
  return (
    <svg viewBox="0 0 24 24" className="size-5 text-indigo-400" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <rect x="3" y="4" width="18" height="13" rx="2" />
      <path d="M8 20h8M12 17v3M7 9.5l2.5 2.5L7 14.5M12 14.5h4" />
    </svg>
  );
}
