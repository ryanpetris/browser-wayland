// Viewer statistics, media controls, and this viewer's codec and quality choice.
import { Activity, Camera, CameraOff, ClipboardCheck, Download, Lock, Mic, MicOff, Volume2, VolumeX } from 'lucide-react';
import { useStore } from '../store.js';
import { PRESETS, TRANSPORTS } from '../protocol.js';
import { codecName } from './ui.jsx';
import { downloadClipboardFile } from '../api.js';

const PRESET_LABEL = { 'very-low': 'Very Low', low: 'Low', medium: 'Medium', high: 'High', max: 'Max' };
const mbit = (kbps, digits = 3) => `${Number((kbps / 1000).toFixed(digits))} Mbit/s`;

// The selected ceiling and current encoder target are separate from network throughput.
function Choice({ viewer }) {
  const st = useStore(viewer.store, s => s.streamState);
  const choice = useStore(viewer.store, s => s.choice);
  const codecs = useStore(viewer.store, s => s.codecs);
  const decodable = useStore(viewer.store, s => s.decodable);
  const status = useStore(viewer.store, s => s.status);
  const transport = useStore(viewer.store, s => s.transport);
  const rtcAvailable = useStore(viewer.store, s => s.rtcAvailable);
  if (status !== 'connected') return null;
  const TRANSPORT_LABEL = { webrtc: 'WebRTC', websocket: 'WebSocket' };
  const ceilings = { 'very-low': 2000, low: 5000, medium: st?.medium_kbps, high: 12000, max: 25000 };
  if (st?.preset) ceilings[st.preset] = st.ceiling_kbps;
  const both = codecs.filter(c => decodable.includes(c.codec));
  const cls = 'rounded border border-zinc-700 bg-zinc-800 px-1 py-0.5 text-[11px] text-zinc-300 focus:outline-none';
  return (
    <>
      <select value={choice.codec} onChange={e => viewer.setChoice({ codec: e.target.value })} className={cls} title="Video codec">
        <option value="auto">Auto{st?.auto_codec ? ` (${codecName(st.codec)})` : ''}</option>
        {both.map(c => <option key={c.codec} value={c.codec}>{codecName(c.codec)}{c.hardware ? '' : ' (software)'}</option>)}
      </select>
      <select value={choice.quality} onChange={e => viewer.setChoice({ quality: e.target.value })} className={cls} title="Quality">
        {PRESETS.map(p => <option key={p} value={p}>{PRESET_LABEL[p]} ({ceilings[p] === undefined ? 'server ceiling' : `up to ${mbit(ceilings[p])}`})</option>)}
      </select>
      <span className="inline-block w-[30ch] shrink-0 whitespace-nowrap" title="Current encoder target; actual network throughput depends on scene activity">
        {st?.preset === choice.quality ? `Target ${mbit(st.bitrate_kbps, 1)}${st.max_fps ? `, ${st.max_fps} fps cap` : ''}` : 'Applying quality…'}
      </span>
      {(rtcAvailable || transport === 'webrtc') && (
        <select value={transport} onChange={e => viewer.setTransport(e.target.value)} className={cls} title="Transport: how the video travels (the socket unless the data channel is picked and opens)">
          {TRANSPORTS.map(t => <option key={t} value={t}>{TRANSPORT_LABEL[t]}</option>)}
        </select>
      )}
    </>
  );
}

export function StatusBar({ viewer, audioPanel, onAudioPanel, mixerPanel, onMixer }) {
  const s = useStore(viewer.store, st => st.stats);
  const clipboardText = useStore(viewer.store, st => st.clipboardText);
  const clipboardFiles = useStore(viewer.store, st => st.clipboardFiles);
  const renderer = useStore(viewer.store, st => st.renderer);
  const mic = useStore(viewer.store, st => st.mic);
  const micAvailable = useStore(viewer.store, st => st.micAvailable);
  const cam = useStore(viewer.store, st => st.cam);
  const camAvailable = useStore(viewer.store, st => st.camAvailable);
  const role = useStore(viewer.store, st => st.role);
  const transport = useStore(viewer.store, st => st.transport);
  const videoVia = useStore(viewer.store, st => st.videoVia);
  const status = useStore(viewer.store, st => st.status);
  const recovery = useStore(viewer.store, st => st.rtcRecovery);
  const transportHint = status !== 'connected' ? 'WebSocket disconnected'
    : videoVia === 'webrtc' ? 'WebRTC'
    : transport !== 'webrtc' ? 'WebSocket'
    : recovery.state === 'unavailable' ? 'WebSocket · WebRTC unavailable'
    : recovery.state === 'connecting' ? 'WebSocket · connecting WebRTC'
    : 'WebSocket · retrying WebRTC';
  const bad = s.lost + s.dropped + s.decodeErrors;
  // Fixed readout widths keep live statistics from resizing the stage.
  return (
    <footer className="relative flex min-h-7 shrink-0 flex-wrap items-center gap-x-4 gap-y-1 border-t border-zinc-800 bg-zinc-900 px-3 py-0.5 font-mono text-[11px] text-zinc-500">
      <MouseCaptureHint viewer={viewer} className="absolute inset-0 z-20 flex" />
      <span className="flex w-[10ch] shrink-0 items-center gap-1.5 overflow-hidden whitespace-nowrap" title={`${s.fps} fps`}><Activity className="size-3" /> {s.fps} fps</span>
      <span className="w-[17ch] shrink-0 truncate" title={`Measured video throughput: ${s.mbps.toFixed(1)} Mbit/s`}>{s.mbps.toFixed(1)} Mbit/s</span>
      <span className="hidden w-[10ch] shrink-0 truncate sm:inline" title={`Input to the next painted frame: ${s.latencyMs.toFixed(0)} ms`}>{s.latencyMs.toFixed(0)} ms</span>
      <span className={`hidden w-[20ch] shrink-0 truncate sm:inline ${bad ? 'text-amber-400' : ''}`} title={`lost ${s.lost} · dropped ${s.dropped} · decode errors ${s.decodeErrors}`}>{s.lost} · {s.dropped} · {s.decodeErrors}</span>
      <span className="flex w-[40ch] max-w-full shrink-0 items-center gap-2" title={recovery.reason || transportHint}>
        <span className="min-w-0 flex-1 truncate" data-transport-status>{transportHint}</span>
        <button type="button" onClick={() => viewer.retryRtc()} disabled={recovery.state !== 'waiting'} className={`shrink-0 text-indigo-300 hover:text-indigo-200 ${recovery.state === 'waiting' ? '' : 'invisible'}`}>Retry now</button>
      </span>
      <span className="ml-auto flex min-w-0 flex-wrap items-center justify-end gap-x-4 gap-y-1">
        {clipboardFiles.length > 0 && ['controller', 'participant'].includes(role) ? (
          <button type="button" onClick={async () => { for (const [i, n] of clipboardFiles.entries()) await downloadClipboardFile(i, n).catch(() => {}); }} title={`Download the copied files: ${clipboardFiles.join(', ')}`} className="flex max-w-48 items-center gap-1 truncate text-indigo-300 hover:text-indigo-200">
            <Download className="size-3 shrink-0" /><span className="truncate">{clipboardText} copied</span>
          </button>
        ) : clipboardText && <span className="flex max-w-48 items-center gap-1 truncate" title={`Clipboard: ${clipboardText}`}><ClipboardCheck className="size-3 shrink-0" /><span className="truncate">{clipboardText}</span></span>}
        {onMixer && <button id="session-mixer-toggle" type="button" aria-label="Session audio mixer" aria-expanded={mixerPanel} onClick={onMixer} className="text-indigo-300 hover:text-indigo-200">Mixer</button>}
        {onAudioPanel && <button type="button" aria-label="Audio visualiser" aria-expanded={audioPanel} onClick={onAudioPanel} className="text-indigo-300 hover:text-indigo-200">Visualiser</button>}
        <span className="flex items-center gap-1" title={s.audio ? `audio ${s.audio.state}` : 'no audio yet'}>
          {s.audio?.state === 'running' ? <Volume2 className="size-3 text-emerald-400" /> : <VolumeX className="size-3" />}
        </span>
        {role === 'controller' && micAvailable && navigator.mediaDevices && 'AudioEncoder' in window && (
          <button type="button" aria-label="Microphone" aria-pressed={mic} onClick={e => { (mic ? viewer.mic.stop : viewer.mic.start)(); e.currentTarget.blur(); }} title={mic ? 'Microphone on: the desktop hears you' : 'Microphone: let the desktop hear you'} className="flex items-center hover:text-zinc-300">
            {mic ? <Mic className="size-3 text-emerald-400" /> : <MicOff className="size-3" />}
          </button>
        )}
        {role === 'controller' && camAvailable && navigator.mediaDevices && 'VideoEncoder' in window && 'MediaStreamTrackProcessor' in window && (
          <button type="button" aria-label="Webcam" aria-pressed={cam} onClick={e => { (cam ? viewer.cam.stop : viewer.cam.start)(); e.currentTarget.blur(); }} title={cam ? 'Webcam on: the desktop sees you' : 'Webcam: let the desktop see you'} className="flex items-center hover:text-zinc-300">
            {cam ? <Camera className="size-3 text-emerald-400" /> : <CameraOff className="size-3" />}
          </button>
        )}
        <span className="hidden min-w-0 flex-wrap items-center justify-end gap-2 sm:flex"><Choice viewer={viewer} /></span>
        <span className="hidden sm:inline">{renderer}</span>
      </span>
    </footer>
  );
}

export function MouseCaptureHint({ viewer, className = '' }) {
  const locked = useStore(viewer.store, s => s.locked);
  if (!locked) return null;
  return <div role="status" className={`mouse-capture-hint pointer-events-none items-center justify-center gap-2 bg-amber-300 px-3 py-1 font-sans text-sm font-semibold text-zinc-950 ${className}`}>
    <Lock className="size-4 shrink-0" /> Press Left Ctrl + Left Alt to release mouse
  </div>;
}
