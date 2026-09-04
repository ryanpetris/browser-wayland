// The viewer: chrome around the stage. State comes from the engine (viewer.js) through its store.
import { useEffect, useRef, useState } from 'react';
import { createViewer } from './viewer.js';
import { useStore } from './store.js';
import { WINDOW, pref } from './api.js';
import { TopBar } from './components/TopBar.jsx';
import { Stage } from './components/Stage.jsx';
import { Sidebar } from './components/Sidebar.jsx';
import { StatusBar } from './components/StatusBar.jsx';
import { TokenForm } from './components/TokenForm.jsx';

const viewer = createViewer();

// A remembered on/off switch.
function usePref(key, fallback) {
  const [on, set] = useState(() => pref.get(key, fallback));
  return [on, v => { set(v); pref.set(key, v); }];
}

export function App() {
  const status = useStore(viewer.store, s => s.status);
  const [sidebar, setSidebar] = usePref('sidebar', true);
  const [borders, setBorders] = usePref('borders', false);
  const [elements, setElements] = usePref('elements', false);
  const [tab, setTab] = useState('windows');
  const stageRef = useRef(null);
  useEffect(() => viewer.setElementsOn(elements && !WINDOW), [elements]);
  useEffect(() => viewer.setStatsOn(sidebar && tab === 'stats'), [sidebar, tab]);
  const windowMode = !!WINDOW;

  return (
    <div className="flex h-full w-full flex-col overflow-hidden bg-zinc-950 text-zinc-300 select-none">
      <TopBar
        viewer={viewer}
        windowMode={windowMode}
        borders={borders} onBorders={() => setBorders(!borders)}
        elements={elements} onElements={() => setElements(!elements)}
        sidebar={sidebar} onSidebar={() => setSidebar(!sidebar)}
        onFullscreen={() => stageRef.current && viewer.fullscreen(stageRef.current)}
      />
      <div className="flex min-h-0 flex-1">
        <Stage ref={stageRef} viewer={viewer} windowMode={windowMode} borders={borders && !windowMode} elements={elements && !windowMode} />
        {sidebar && !windowMode && <Sidebar viewer={viewer} tab={tab} onTab={setTab} />}
      </div>
      <StatusBar viewer={viewer} />
      {(status === 'no-token' || status === 'unauthorized') && <TokenForm viewer={viewer} />}
    </div>
  );
}
