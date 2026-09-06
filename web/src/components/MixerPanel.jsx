import { useEffect, useRef, useState } from 'react';
import { useStore } from '../store.js';

const button = 'rounded border border-zinc-600 px-2 py-1 hover:bg-zinc-700 disabled:opacity-50 disabled:hover:bg-transparent focus-visible:outline-2 focus-visible:outline-indigo-400';
const directions = { output: 'Output', input: 'Input', playback: 'Playback', recording: 'Recording' };

function MixerRow({ viewer, node, nodes, controls, routing, error }) {
  const peak = useStore(viewer.store, s => s.mixerLevels[node.id] ?? 0);
  const [draft, setDraft] = useState(null);
  const [localError, setLocalError] = useState('');
  const timer = useRef(0), volume = useRef(node.volume);
  volume.current = node.volume;
  useEffect(() => {
    setDraft(null); setLocalError(''); clearTimeout(timer.current);
    return () => clearTimeout(timer.current);
  }, [node.id, controls, error]);
  useEffect(() => {
    if (draft !== null && Math.abs((node.volume ?? 0) - draft) < 0.6) { setDraft(null); clearTimeout(timer.current); }
  }, [node.volume, draft]);

  function changeVolume(value) {
    setLocalError('');
    if (!viewer.mixer.command({ op: 'volume', id: node.id, value })) { setDraft(null); return; }
    setDraft(value); clearTimeout(timer.current);
    timer.current = setTimeout(() => {
      setDraft(null);
      if (Math.abs((volume.current ?? 0) - value) >= 0.6) setLocalError('Volume change was not confirmed.');
    }, 1500);
  }

  const targetKind = node.kind === 'playback' ? 'output' : node.kind === 'recording' ? 'input' : null;
  const targets = nodes.filter(n => n.kind === targetKind);
  const endpoints = nodes.filter(n => n.kind === node.kind);
  const route = node.targets.map(id => nodes.find(n => n.id === id)?.name).filter(Boolean).join(', ');
  const level = !node.meter_active ? 'Inactive' : peak > 0.00001 ? `${(20 * Math.log10(peak)).toFixed(1)} dBFS` : 'Silent';
  return (
    <article role="group" aria-label={`${node.name} ${directions[node.kind]}`} data-audio-id={node.id} className="rounded border border-zinc-700 bg-zinc-900 p-3">
      <div className="flex flex-wrap items-center gap-2">
        <h3 className="min-w-0 flex-1 break-words font-medium text-zinc-100">{node.name}</h3>
        <span className="text-xs text-zinc-400">{directions[node.kind]} · {node.state}</span>
      </div>
      <div className="mt-2 flex flex-wrap items-center gap-3">
        {node.volume !== null ? <label className="flex min-w-40 flex-1 items-center gap-2">
          <span className="text-xs">Volume</span>
          <input type="range" min="0" max="100" step="1" aria-label={`${node.name} volume`} className="min-w-12 flex-1 accent-indigo-400 disabled:opacity-40"
            value={draft ?? Math.round(node.volume)} disabled={!controls || !node.volume_writable}
            onChange={event => changeVolume(Number(event.target.value))} />
          <output className="w-10 text-right text-xs tabular-nums">{Math.round(draft ?? node.volume)}%</output>
        </label> : <span className="text-xs text-zinc-400">Volume control unavailable</span>}
        {node.mute !== null ? <button type="button" className={button} aria-label={`${node.name} mute`} aria-pressed={node.mute}
          disabled={!controls || !node.mute_writable} onClick={() => viewer.mixer.command({ op: 'mute', id: node.id, value: !node.mute })}>
          {node.mute ? 'Unmute' : 'Mute'}
        </button> : <span className="text-xs text-zinc-400">Mute control unavailable</span>}
        {!targetKind && endpoints.length > 1 && <button type="button" className={button} disabled={!controls || !routing || !node.routing_writable || node.is_default}
          onClick={() => viewer.mixer.command({ op: 'default', id: node.id })}>{node.is_default ? 'Default' : 'Make default'}</button>}
      </div>
      {controls && ((!node.volume_writable && node.volume !== null) || (!node.mute_writable && node.mute !== null)) && <p className="mt-1 text-xs text-zinc-400">Disabled controls are read-only for this audio object.</p>}
      {targetKind && <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-zinc-400">
        <span>{directions[targetKind]}: {route || 'Not linked'}</span>
        {targets.length > 1 && <label>Target <select aria-label={`${node.name} target`} className="max-w-full rounded border border-zinc-600 bg-zinc-800 p-1 text-zinc-200 disabled:opacity-50"
          disabled={!controls || !routing || !node.routing_writable} value={node.targets.length === 1 ? node.targets[0] : ''}
          onChange={event => viewer.mixer.command({ op: 'target', id: node.id, target: event.target.value || null })}>
          <option value="">Follow session default</option>
          {targets.map(target => <option key={target.id} value={target.id}>{target.name}{target.is_default ? ' (default)' : ''}</option>)}
        </select></label>}
        {targets.length > 1 && (!routing || !node.routing_writable) && <span>Routing control unavailable</span>}
      </div>}
      <div className="mt-2 flex items-center gap-2 text-xs text-zinc-400">
        <meter min="0" max="1" value={node.meter_active ? Math.min(1, peak) : 0} aria-label={`${node.name} peak level`} aria-valuetext={level} className="h-3 min-w-12 flex-1" />
        <span className="w-20 text-right tabular-nums">{level}</span>
      </div>
      <p className="text-[11px] text-zinc-500">{node.meter_before_volume ? 'Before stream volume and mute' : 'After volume and mute'}</p>
      {node.meter_error && <p className="mt-1 text-xs text-amber-300">Meter unavailable: {node.meter_error}</p>}
      {localError && <p role="alert" className="mt-1 text-xs text-amber-300">{localError}</p>}
    </article>
  );
}

export function MixerPanel({ viewer, hidden, onClose }) {
  const snapshot = useStore(viewer.store, s => s.mixer);
  const error = useStore(viewer.store, s => s.mixerError);
  const role = useStore(viewer.store, s => s.role);
  const status = useStore(viewer.store, s => s.status);
  const [visible, setVisible] = useState(() => !document.hidden);
  const close = useRef(null);
  useEffect(() => {
    const visibility = () => setVisible(!document.hidden);
    document.addEventListener('visibilitychange', visibility);
    close.current?.focus();
    return () => document.removeEventListener('visibilitychange', visibility);
  }, []);
  useEffect(() => {
    viewer.mixer.subscribe(!hidden && visible && status === 'connected');
    return () => viewer.mixer.subscribe(false);
  }, [viewer, hidden, visible, status]);
  const controls = role === 'controller' && status === 'connected' && snapshot.available;
  const groups = new Map();
  for (const node of snapshot.nodes) {
    const name = node.kind === 'output' || node.kind === 'input' ? 'Session devices' : node.application || 'Other applications';
    if (!groups.has(name)) groups.set(name, []);
    groups.get(name).push(node);
  }
  return (
    <section aria-label="Session audio mixer" hidden={hidden} className="max-h-[45vh] shrink-0 overflow-y-auto border-t border-zinc-700 bg-zinc-950 p-3 text-sm">
      <div className="flex items-center gap-3">
        <h2 className="font-semibold text-zinc-100">Session audio mixer</h2>
        <button ref={close} type="button" className={`${button} ml-auto`} onClick={onClose}>Close mixer</button>
      </div>
      <p className="mt-1 text-xs text-zinc-400">Changes affect all viewers. Each application row is one audio stream. Use the microphone button to control browser capture.</p>
      {role !== 'controller' && <p className="mt-2 text-xs text-amber-200">Only the controlling viewer can make changes.
        {role === 'participant' && <button type="button" className={`${button} ml-2`} onClick={viewer.takeControl}>Take control</button>}
      </p>}
      {snapshot.available && snapshot.error && <p role="alert" className="mt-2 text-amber-300">{snapshot.error}</p>}
      {error && <p role="alert" className="mt-2 text-amber-300">{error}</p>}
      {status !== 'connected' ? <p className="mt-3">Waiting for the desktop connection.</p>
        : !snapshot.available ? <p className="mt-3">{snapshot.error || 'Connecting to session audio…'}</p>
        : [...groups].map(([name, nodes]) => <div key={name} className="mt-3">
          <h2 className="mb-2 text-xs font-semibold text-zinc-400">{name}</h2>
          <div className="grid gap-2 md:grid-cols-2 xl:grid-cols-3">
            {nodes.map(node => <MixerRow key={node.id} viewer={viewer} node={node} nodes={snapshot.nodes} controls={controls} routing={snapshot.routing} error={error} />)}
          </div>
        </div>)}
    </section>
  );
}
