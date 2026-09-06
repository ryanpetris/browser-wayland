import { Expand, Eye, Hand, Keyboard, LayoutGrid, LayoutList, MousePointer2, PanelRight, Power, Settings } from 'lucide-react';
import { useStore } from '../store.js';
import { WINDOW } from '../api.js';
import { IconButton, codecName } from './ui.jsx';

const STATUS = {
  'no-token': ['bg-zinc-500', 'No token'],
  connecting: ['bg-amber-400 animate-pulse', 'Connecting…'],
  connected: ['bg-emerald-400', 'Connected'],
  retrying: ['bg-amber-400 animate-pulse', 'Reconnecting…'],
  unauthorized: ['bg-rose-400', 'Not authorized'],
  gone: ['bg-zinc-500', 'Window closed'],
  quit: ['bg-zinc-500', 'Shut down'],
};

export function TopBar({ viewer, windowMode, sidebar, onSidebar, onFullscreen, menu, onMenu, keyboard, onKeyboard }) {
  const status = useStore(viewer.store, s => s.status);
  const stream = useStore(viewer.store, s => s.stream);
  const windowTitle = useStore(viewer.store, s => s.windowTitle);
  const role = useStore(viewer.store, s => s.role);
  const touchMouse = useStore(viewer.store, s => s.touchMouse);
  const [dot, text] = STATUS[status];
  const acts = !windowMode && role && role !== 'viewer' && status === 'connected'; // the menus act on the desktop
  return (
    <header onClick={event => { if (!event.target.closest('[data-menu-trigger]')) onMenu(null); }} className="flex h-11 shrink-0 items-center gap-2 border-b border-zinc-800 bg-zinc-900 px-2 sm:gap-3 sm:px-3">
      <div className="flex min-w-0 shrink items-center gap-2 font-medium text-zinc-100">
        <Logo />
        <span className="hidden truncate sm:inline">{windowMode ? windowTitle || `Window ${WINDOW}` : 'browser-wayland'}</span>
      </div>
      {acts && <IconButton data-menu-trigger icon={LayoutGrid} label="Applications" active={menu === 'apps'} onClick={() => onMenu('apps')} />}
      <div className="flex min-w-0 shrink-0 items-center gap-2 text-xs text-zinc-400">
        <span className={`size-2 shrink-0 rounded-full ${dot}`} title={text} />
        <span className="hidden truncate sm:inline">{text}</span>
        {stream && status === 'connected' && (
          <span className="hidden truncate font-mono text-zinc-500 sm:inline">
            · {codecName(stream.codec)} {stream.width}×{stream.height}{stream.scale !== 1 ? ` @${stream.scale.toFixed(2)}` : ''}
          </span>
        )}
      </div>
      <div className="ml-auto flex items-center gap-1">
        {!windowMode && role === 'participant' && (
          <button type="button" onClick={e => { viewer.takeControl(); e.currentTarget.blur(); }} title="Drive the desktop; it takes this window's size" className="mr-1 inline-flex items-center gap-1.5 rounded-md bg-indigo-500 px-2.5 py-1 text-xs font-medium text-white hover:bg-indigo-400">
            <MousePointer2 className="size-3.5" /> <span className="hidden sm:inline">Take control</span>
          </button>
        )}
        {role === 'viewer' && (
          <span className="mr-1 inline-flex items-center gap-1.5 rounded-md border border-zinc-700 px-2 py-1 text-xs text-zinc-400" title="The viewer token watches; it can't act"><Eye className="size-3.5" /> <span className="hidden sm:inline">view only</span></span>
        )}
        {!windowMode && role === 'controller' && <span className="mr-1 hidden text-xs text-emerald-400/80 sm:inline" title="Your pointer, keyboard and window size are the desktop's">controlling</span>}
        {!windowMode && viewer.touch && role === 'controller' && <IconButton icon={Keyboard} label="On-screen keyboard" active={keyboard} onClick={onKeyboard} />}
        {!windowMode && viewer.touch && <IconButton icon={Hand} label="Touch as mouse: tap, hold for the right button, two fingers scroll, pinch to zoom (off: applications get the touch points)" active={touchMouse} onClick={() => viewer.setTouchMouse(!touchMouse)} />}
        {!windowMode && (
          <>
            <IconButton data-menu-trigger id="settings-toggle" icon={Settings} label="Settings" active={menu === 'settings'} aria-haspopup="dialog" aria-expanded={menu === 'settings'} aria-controls="viewer-settings" onClick={() => onMenu('settings')} />
            <IconButton icon={sidebar ? PanelRight : LayoutList} label="Windows and statistics" active={sidebar} onClick={onSidebar} />
            <span className="mx-1 h-5 w-px bg-zinc-800" />
          </>
        )}
        <IconButton icon={Expand} label="Fullscreen (browser shortcuts go to the desktop)" onClick={onFullscreen} />
        {acts && <IconButton data-menu-trigger icon={Power} label="Quit browser-wayland" active={menu === 'power'} onClick={() => onMenu('power')} className="hover:text-rose-300" />}
      </div>
    </header>
  );
}

function Logo() {
  return (
    <svg viewBox="0 0 24 24" className="size-5 shrink-0 text-indigo-400" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <rect x="3" y="4" width="18" height="13" rx="2" />
      <path d="M8 20h8M12 17v3M7 9.5l2.5 2.5L7 14.5M12 14.5h4" />
    </svg>
  );
}
