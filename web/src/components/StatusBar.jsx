// One line of numbers under the display.
import { Activity, ClipboardCheck, Lock, Volume2, VolumeX } from 'lucide-react';
import { useStore } from '../store.js';

export function StatusBar({ viewer }) {
  const s = useStore(viewer.store, st => st.stats);
  const locked = useStore(viewer.store, st => st.locked);
  const clipboardText = useStore(viewer.store, st => st.clipboardText);
  const renderer = useStore(viewer.store, st => st.renderer);
  const bad = s.lost + s.dropped + s.decodeErrors;
  return (
    <footer className="flex h-7 shrink-0 items-center gap-4 border-t border-zinc-800 bg-zinc-900 px-3 font-mono text-[11px] text-zinc-500">
      <span className="flex items-center gap-1.5"><Activity className="size-3" /> {s.fps} fps</span>
      <span>{s.mbps.toFixed(1)} Mbit/s</span>
      <span title="Input to the next painted frame">{s.latencyMs.toFixed(0)} ms</span>
      <span className={bad ? 'text-amber-400' : ''} title="lost · dropped · decode errors">{s.lost} · {s.dropped} · {s.decodeErrors}</span>
      <span className="ml-auto flex items-center gap-4">
        {clipboardText && <span className="flex max-w-48 items-center gap-1 truncate" title={`Clipboard: ${clipboardText}`}><ClipboardCheck className="size-3 shrink-0" /><span className="truncate">{clipboardText}</span></span>}
        {locked && <span className="flex items-center gap-1 text-indigo-300"><Lock className="size-3" /> pointer locked</span>}
        <span className="flex items-center gap-1" title={s.audio ? `audio ${s.audio.state}` : 'no audio yet'}>
          {s.audio?.state === 'running' ? <Volume2 className="size-3 text-emerald-400" /> : <VolumeX className="size-3" />}
        </span>
        <span>{renderer}</span>
      </span>
    </footer>
  );
}
