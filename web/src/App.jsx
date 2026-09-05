// The viewer: chrome around the stage. State comes from the engine (viewer.js) through its store.
import { useEffect, useState } from 'react';
import { useStore } from './store.js';
import { WINDOW, pref } from './api.js';
import { TopBar } from './components/TopBar.jsx';
import { Stage } from './components/Stage.jsx';
import { Sidebar } from './components/Sidebar.jsx';
import { StatusBar } from './components/StatusBar.jsx';
import { TokenForm } from './components/TokenForm.jsx';
import { Launcher, PowerMenu } from './components/Launcher.jsx';

// A remembered on/off switch.
function usePref(key, fallback) {
  const [on, set] = useState(() => pref.get(key, fallback));
  return [on, v => { set(v); pref.set(key, v); }];
}

export function App({ viewer }) {
  const status = useStore(viewer.store, s => s.status);
  const [sidebar, setSidebar] = usePref('sidebar', true);
  const [borders, setBorders] = usePref('borders', false);
  const [elements, setElements] = usePref('elements', false);
  const [tab, setTab] = useState('windows');
  const [menu, setMenu] = useState(null); // 'apps' or 'power' while one of the top bar's menus is open
  const [fullscreen, setFullscreen] = useState(false); // the chrome is gone then, so nothing is collected for it
  const windowMode = !!WINDOW;
  useEffect(() => {
    const on = () => setFullscreen(!!document.fullscreenElement);
    document.addEventListener('fullscreenchange', on);
    return () => document.removeEventListener('fullscreenchange', on);
  }, []);
  useEffect(() => viewer.setElementsOn(elements && !windowMode), [viewer, elements, windowMode]);
  useEffect(() => viewer.setStatsOn(sidebar && tab === 'stats' && !fullscreen), [viewer, sidebar, tab, fullscreen]);

  return (
    <div className="relative flex h-full w-full flex-col overflow-hidden bg-zinc-950 text-zinc-300 select-none">
      <TopBar
        viewer={viewer}
        windowMode={windowMode}
        borders={borders} onBorders={() => setBorders(!borders)}
        elements={elements} onElements={() => setElements(!elements)}
        sidebar={sidebar} onSidebar={() => setSidebar(!sidebar)}
        onFullscreen={viewer.fullscreen}
        menu={menu} onMenu={m => setMenu(menu === m ? null : m)}
      />
      {menu === 'apps' && <Launcher viewer={viewer} onClose={() => setMenu(null)} />}
      {menu === 'power' && <PowerMenu viewer={viewer} onClose={() => setMenu(null)} />}
      <div className="flex min-h-0 flex-1">
        <Stage viewer={viewer} windowMode={windowMode} borders={borders && !windowMode} elements={elements && !windowMode} />
        {/* stays mounted while hidden, so the thumbnails don't reload on every toggle */}
        {!windowMode && <Sidebar viewer={viewer} tab={tab} onTab={setTab} hidden={!sidebar} />}
      </div>
      <StatusBar viewer={viewer} />
      {(status === 'no-token' || status === 'unauthorized') && <TokenForm viewer={viewer} />}
    </div>
  );
}
