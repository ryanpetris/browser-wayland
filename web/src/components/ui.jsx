// Small shared pieces: toolbar buttons and the colour of a window.

/// An icon button; `active` marks a toggle that is on. Focus leaves it after a click so keys go to the desktop.
export function IconButton({ icon: Icon, label, active = false, onClick, className = '' }) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      aria-pressed={active}
      onClick={e => { onClick?.(e); e.currentTarget.blur(); }}
      className={`inline-flex size-8 items-center justify-center rounded-md transition-colors hover:bg-zinc-800 hover:text-zinc-100 focus-visible:outline-2 focus-visible:outline-indigo-400 ${
        active ? 'bg-indigo-500/15 text-indigo-300 ring-1 ring-indigo-500/40 ring-inset' : 'text-zinc-400'
      } ${className}`}
    >
      <Icon className="size-4" strokeWidth={1.75} />
    </button>
  );
}

const CODEC = { avc1: 'H.264', hev1: 'HEVC', hvc1: 'HEVC', vp09: 'VP9', av01: 'AV1', h264: 'H.264', hevc: 'HEVC', vp9: 'VP9', av1: 'AV1', vp8: 'VP8' };
/// The family of a WebCodecs codec string.
export const codecName = c => CODEC[c?.split('.')[0]] ?? c;

// One hue per app id, so every window of an app gets the same colour (also used for the border overlay).
export function hue(s) {
  let h = 0;
  for (const c of s) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return h % 360;
}
export const windowColor = w => `hsl(${hue(w.app_id || w.title)} 70% 58%)`;
