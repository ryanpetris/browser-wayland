import { useEffect, useRef, useState } from 'react';
import { pref } from '../api.js';
import { useStore } from '../store.js';

// This import is eliminated entirely in builds without the optional renderer.
const loadRenderer = __BW_VISUALISER__ ? () => import('../visualiser.js') : null;

export function AudioPanel({ viewer, hidden, onClose }) {
  const panel = useRef(null), canvas = useRef(null), renderer = useRef(null);
  const playback = useStore(viewer.store, s => s.playback);
  const audio = useStore(viewer.store, s => s.stats.audio);
  const status = useStore(viewer.store, s => s.status);
  const available = useStore(viewer.store, s => s.audioAvailable);
  const [style, setStyle] = useState(() => pref.getStr('visualiser.style', 'bars'));
  const [gradient, setGradient] = useState(() => pref.getStr('visualiser.gradient', 'classic'));
  const [animate, setAnimate] = useState(() => pref.get('visualiser.animate', true));
  const [reduced, setReduced] = useState(() => matchMedia('(prefers-reduced-motion: reduce)').matches);
  const [visible, setVisible] = useState(() => !document.hidden);
  const [expanded, setExpanded] = useState(false);
  const [fullscreen, setFullscreen] = useState(false);
  const [error, setError] = useState('');
  const [ready, setReady] = useState(false);
  const paused = hidden || !visible || !animate || reduced || status !== 'connected';
  const options = useRef({});
  useEffect(() => { options.current = { style, gradient, paused }; }, [style, gradient, paused]);

  useEffect(() => {
    const media = matchMedia('(prefers-reduced-motion: reduce)');
    const motion = () => setReduced(media.matches);
    const visibility = () => setVisible(!document.hidden);
    const full = () => setFullscreen(document.fullscreenElement === panel.current);
    media.addEventListener('change', motion);
    document.addEventListener('visibilitychange', visibility);
    document.addEventListener('fullscreenchange', full);
    return () => {
      media.removeEventListener('change', motion);
      document.removeEventListener('visibilitychange', visibility);
      document.removeEventListener('fullscreenchange', full);
    };
  }, []);

  useEffect(() => {
    if (!playback || !loadRenderer) return;
    let cancelled = false, instance;
    setError(''); setReady(false);
    loadRenderer().then(({ createVisualiser }) => {
      if (cancelled) return;
      instance = createVisualiser(canvas.current, playback);
      renderer.current = instance;
      const { style, gradient, paused } = options.current;
      instance.style(style, gradient);
      instance.pause(paused);
      setReady(true);
    }).catch(() => fail());
    function fail() {
      instance?.dispose();
      if (renderer.current === instance) renderer.current = null;
      instance = null;
      if (!cancelled) { setError('Visualiser unavailable. Desktop playback is unchanged.'); setReady(false); }
    }
    return () => {
      cancelled = true;
      instance?.dispose();
      if (renderer.current === instance) renderer.current = null;
    };
  }, [playback]);

  useEffect(() => {
    try {
      renderer.current?.style(style, gradient);
      renderer.current?.pause(paused);
    } catch {
      renderer.current?.dispose(); renderer.current = null;
      setError('Visualiser unavailable. Desktop playback is unchanged.');
    }
  }, [style, gradient, paused]);

  const message = status !== 'connected' ? 'Waiting for the desktop connection.'
    : !playback ? available ? 'Waiting for session audio.' : 'Session audio unavailable.'
    : audio?.state === 'suspended' ? 'Playback is waiting for a user gesture.'
    : audio?.signalPeak > 0.0001 ? 'Session signal received.' : 'Connected, but silent.';
  const button = 'rounded border border-zinc-600 px-2 py-1 hover:bg-zinc-700 focus-visible:outline-2 focus-visible:outline-indigo-400';
  const select = 'rounded border border-zinc-600 bg-zinc-800 px-2 py-1';
  return (
    <section ref={panel} hidden={hidden} aria-label="Session audio" className="shrink-0 border-t border-zinc-700 bg-zinc-900 p-3 text-xs">
      <div className="flex flex-wrap items-center gap-3">
        <strong>Session audio</strong>
        <label>Style <select className={select} value={style} onChange={e => { setStyle(e.target.value); pref.setStr('visualiser.style', e.target.value); }}>
          <option value="bars">Spectrum bars</option><option value="line">Line / area spectrum</option>
          <option value="radial">Radial spectrum</option><option value="stereo">Stereo spectrum</option>
        </select></label>
        <label>Colours <select className={select} value={gradient} onChange={e => { setGradient(e.target.value); pref.setStr('visualiser.gradient', e.target.value); }}>
          <option value="classic">Classic</option><option value="rainbow">Rainbow</option><option value="steelblue">Steel blue</option>
        </select></label>
        <label><input type="checkbox" checked={animate} onChange={e => { setAnimate(e.target.checked); pref.set('visualiser.animate', e.target.checked); }} /> Animate</label>
        <button type="button" className={button} onClick={() => setExpanded(!expanded)} aria-expanded={expanded}>{expanded ? 'Collapse' : 'Expand'}</button>
        {document.fullscreenEnabled && <button type="button" className={button} onClick={() => {
          const action = fullscreen ? document.exitFullscreen() : panel.current.requestFullscreen();
          action.catch(() => setError('Fullscreen unavailable. You can still expand the panel.'));
        }}>{fullscreen ? 'Exit fullscreen' : 'Fullscreen visualiser'}</button>}
        <button type="button" onClick={onClose} className={`${button} ml-auto`}>Close visualiser</button>
      </div>
      <p role="status" className="my-2">{message} {reduced ? 'Animation paused for reduced motion.' : !animate ? 'Animation off.' : ''}
        {audio?.state === 'suspended' && <button type="button" onClick={viewer.resumeAudio} className="ml-2 underline">Start playback</button>}
      </p>
      {error && <p role="alert">{error}</p>}
      {playback && !ready && !error && <p>Loading visualiser…</p>}
      <div ref={canvas} className="w-full overflow-hidden" style={{ height: fullscreen ? 'calc(100vh - 160px)' : expanded ? '35vh' : '130px' }} aria-hidden="true" />
      <p className="mt-2 text-zinc-400">Mixed playback at this viewer, before browser or system muting.
        {' '}<a href="/assets/audiomotion-LICENSE.txt" target="_blank" rel="noreferrer" className="underline">audioMotion licence</a>
        {' · '}<a href="/assets/audiomotion-source.js" download className="underline">audioMotion source</a>
        {' · '}<a href="/assets/viewer-source.tar.gz" download className="underline">Corresponding viewer source</a>
      </p>
    </section>
  );
}
