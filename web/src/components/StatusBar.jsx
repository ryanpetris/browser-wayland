// One line of numbers under the display, and this viewer's codec and quality choice.
import { Activity, ClipboardCheck, Download, Lock, Mic, MicOff, Volume2, VolumeX } from 'lucide-react';
import { useStore } from '../store.js';
import { PRESETS } from '../protocol.js';
import { codecName } from './ui.jsx';
import { downloadClipboardFile } from '../api.js';

const PRESET_LABEL = { low: 'Low (2 Mbit/s, 30 fps)', medium: 'Medium (5 Mbit/s)', high: 'High (12 Mbit/s)', max: 'Max (25 Mbit/s)' };
const mbit = kbps => `${(kbps / 1000).toFixed(kbps % 1000 ? 1 : 0)} Mbit/s`;

// "Auto (HEVC)" and "Auto (5 Mbit/s)" show what Auto picked; the other entries are what both sides can do.
function Choice({ viewer }) {
  const st = useStore(viewer.store, s => s.streamState);
  const choice = useStore(viewer.store, s => s.choice);
  const codecs = useStore(viewer.store, s => s.codecs);
  const decodable = useStore(viewer.store, s => s.decodable);
  const status = useStore(viewer.store, s => s.status);
  if (status !== 'connected') return null;
  const both = codecs.filter(c => decodable.includes(c.codec));
  const cls = 'rounded border border-zinc-700 bg-zinc-800 px-1 py-0.5 text-[11px] text-zinc-300 focus:outline-none';
  return (
    <>
      <select value={choice.codec} onChange={e => viewer.setChoice({ codec: e.target.value })} className={cls} title="Video codec">
        <option value="auto">Auto{st?.auto_codec ? ` (${codecName(st.codec)})` : ''}</option>
        {both.map(c => <option key={c.codec} value={c.codec}>{codecName(c.codec)}{c.hardware ? '' : ' (software)'}</option>)}
      </select>
      <select value={choice.quality} onChange={e => viewer.setChoice({ quality: e.target.value })} className={cls} title="Quality">
        <option value="auto">Auto{st?.auto_quality ? ` (${mbit(st.bitrate_kbps)}${st.max_fps ? `, ${st.max_fps} fps` : ''})` : ''}</option>
        {PRESETS.filter(p => p !== 'auto').map(p => <option key={p} value={p}>{PRESET_LABEL[p]}</option>)}
      </select>
    </>
  );
}

export function StatusBar({ viewer }) {
  const s = useStore(viewer.store, st => st.stats);
  const locked = useStore(viewer.store, st => st.locked);
  const clipboardText = useStore(viewer.store, st => st.clipboardText);
  const clipboardFiles = useStore(viewer.store, st => st.clipboardFiles);
  const renderer = useStore(viewer.store, st => st.renderer);
  const mic = useStore(viewer.store, st => st.mic);
  const micAvailable = useStore(viewer.store, st => st.micAvailable);
  const role = useStore(viewer.store, st => st.role);
  const bad = s.lost + s.dropped + s.decodeErrors;
  return (
    <footer className="flex h-7 shrink-0 items-center gap-4 border-t border-zinc-800 bg-zinc-900 px-3 font-mono text-[11px] text-zinc-500">
      <span className="flex items-center gap-1.5"><Activity className="size-3" /> {s.fps} fps</span>
      <span>{s.mbps.toFixed(1)} Mbit/s</span>
      <span className="hidden sm:inline" title="Input to the next painted frame">{s.latencyMs.toFixed(0)} ms</span>
      <span className={`hidden sm:inline ${bad ? 'text-amber-400' : ''}`} title="lost · dropped · decode errors">{s.lost} · {s.dropped} · {s.decodeErrors}</span>
      <span className="ml-auto flex items-center gap-4">
        {clipboardFiles.length > 0 ? (
          <button type="button" onClick={async () => { for (const [i, n] of clipboardFiles.entries()) await downloadClipboardFile(i, n).catch(() => {}); }} title={`Download the copied files: ${clipboardFiles.join(', ')}`} className="flex max-w-48 items-center gap-1 truncate text-indigo-300 hover:text-indigo-200">
            <Download className="size-3 shrink-0" /><span className="truncate">{clipboardText} copied</span>
          </button>
        ) : clipboardText && <span className="flex max-w-48 items-center gap-1 truncate" title={`Clipboard: ${clipboardText}`}><ClipboardCheck className="size-3 shrink-0" /><span className="truncate">{clipboardText}</span></span>}
        {locked && <span className="flex items-center gap-1 text-indigo-300"><Lock className="size-3" /> pointer locked</span>}
        <span className="flex items-center gap-1" title={s.audio ? `audio ${s.audio.state}` : 'no audio yet'}>
          {s.audio?.state === 'running' ? <Volume2 className="size-3 text-emerald-400" /> : <VolumeX className="size-3" />}
        </span>
        {role === 'controller' && micAvailable && navigator.mediaDevices && (
          <button type="button" aria-label="Microphone" aria-pressed={mic} onClick={e => { (mic ? viewer.mic.stop : viewer.mic.start)(); e.currentTarget.blur(); }} title={mic ? 'Microphone on: the desktop hears you' : 'Microphone: let the desktop hear you'} className="flex items-center hover:text-zinc-300">
            {mic ? <Mic className="size-3 text-emerald-400" /> : <MicOff className="size-3" />}
          </button>
        )}
        <span className="hidden items-center gap-4 sm:flex"><Choice viewer={viewer} /></span>
        <span className="hidden sm:inline">{renderer}</span>
      </span>
    </footer>
  );
}
