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
import { About } from './components/About.jsx';
import { Settings } from './components/Settings.jsx';
import { MixerPanel } from './components/MixerPanel.jsx';
import { AudioPanel } from './components/AudioPanel.jsx';
import { Keyboard, focusKeyboard } from './components/Keyboard.jsx';

// A remembered on/off switch.
function usePref(key, fallback) {
  const [on, set] = useState(() => pref.get(key, fallback));
  return [on, v => { set(v); pref.set(key, v); }];
}

export function App({ viewer }) {
  const status = useStore(viewer.store, s => s.status);
  const role = useStore(viewer.store, s => s.role);
  const [sidebar, setSidebar] = usePref('sidebar', matchMedia('(min-width: 48rem)').matches); // a phone starts with the stage alone
  const [audioPanel, setAudioPanel] = useState(false);
  const [mixerPanel, setMixerPanel] = useState(false);
  const [keyboard, setKeyboard] = useState(false);
  const [borders, setBorders] = usePref('borders', false);
  const [elements, setElements] = usePref('elements', false);
  const [tab, setTab] = useState('windows');
  const [menu, setMenu] = useState(null); // One top-bar menu at a time.
  const closeMenu = event => {
    setMenu(null);
    if (event?.type === 'keydown' || event?.detail === 0) document.getElementById(`${menu}-toggle`)?.focus();
  };
  const [fullscreen, setFullscreen] = useState(false); // the chrome is gone then, so nothing is collected for it
  const windowMode = !!WINDOW;
  useEffect(() => {
    const on = () => { const full = viewer.isFullscreen(); setFullscreen(full); if (full) setMenu(null); };
    document.addEventListener('fullscreenchange', on);
    return () => document.removeEventListener('fullscreenchange', on);
  }, [viewer]);
  useEffect(() => viewer.setElementsOn(elements && !windowMode), [viewer, elements, windowMode]);
  useEffect(() => { if (role !== 'controller') setKeyboard(false); }, [role]); // only the controller's typing counts
  useEffect(() => viewer.setStatsOn(sidebar && tab === 'stats' && !fullscreen), [viewer, sidebar, tab, fullscreen]);

  return (
    <div className="relative flex h-full w-full flex-col overflow-hidden bg-zinc-950 text-zinc-300 select-none">
      <TopBar
        viewer={viewer}
        windowMode={windowMode}
        sidebar={sidebar} onSidebar={() => setSidebar(!sidebar)}
        onFullscreen={viewer.fullscreen}
        menu={menu} onMenu={m => setMenu(menu === m ? null : m)}
        keyboard={keyboard} onKeyboard={() => (keyboard ? focusKeyboard() : setKeyboard(true))}
      />
      {menu === 'about' && !fullscreen && <About viewer={viewer} onClose={closeMenu} />}
      {menu === 'apps' && <Launcher viewer={viewer} onClose={closeMenu} />}
      {menu === 'power' && <PowerMenu viewer={viewer} onClose={closeMenu} />}
      {menu === 'settings' && !windowMode && !fullscreen && <Settings viewer={viewer} borders={borders} onBorders={setBorders} elements={elements} onElements={setElements} onClose={closeMenu} />}
      <div className="relative flex min-h-0 flex-1">
        <Stage viewer={viewer} windowMode={windowMode} borders={borders && !windowMode} elements={elements && !windowMode} />
        {/* stays mounted while hidden, so the thumbnails don't reload on every toggle */}
        {!windowMode && <Sidebar viewer={viewer} tab={tab} onTab={setTab} hidden={!sidebar} />}
      </div>
      {keyboard && <Keyboard viewer={viewer} onClose={() => setKeyboard(false)} />}
      {audioPanel && !windowMode && <AudioPanel viewer={viewer} hidden={fullscreen} onClose={() => setAudioPanel(false)} />}
      {mixerPanel && !windowMode && <MixerPanel viewer={viewer} hidden={fullscreen} onClose={() => { setMixerPanel(false); document.getElementById('session-mixer-toggle')?.focus(); }} />}
      <StatusBar mixerPanel={mixerPanel} onMixer={!windowMode ? () => setMixerPanel(!mixerPanel) : undefined} viewer={viewer} audioPanel={audioPanel} onAudioPanel={!windowMode ? () => setAudioPanel(!audioPanel) : undefined} />
      {(status === 'no-token' || status === 'unauthorized') && <TokenForm viewer={viewer} />}
    </div>
  );
}
