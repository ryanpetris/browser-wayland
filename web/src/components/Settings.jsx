import { useEffect, useRef } from 'react';
import { Popover } from './Launcher.jsx';

export function Settings({ viewer, borders, onBorders, elements, onElements, onClose }) {
  const first = useRef(null);
  useEffect(() => {
    viewer.releaseInput();
    if (document.pointerLockElement) document.exitPointerLock();
    first.current?.focus();
  }, [viewer]);
  // Keep keyboard navigation local; Escape returns focus to the Settings button.
  const keyDown = event => {
    event.stopPropagation();
    if (event.key === 'Tab') {
      const inputs = [...event.currentTarget.querySelectorAll('input')];
      const index = inputs.indexOf(document.activeElement);
      event.preventDefault();
      inputs[index < 0 ? (event.shiftKey ? inputs.length - 1 : 0) : (index + (event.shiftKey ? inputs.length - 1 : 1)) % inputs.length].focus();
    }
  };
  const label = 'flex min-h-11 cursor-pointer items-start gap-3 rounded-lg p-2 hover:bg-zinc-800';
  const checkbox = 'mt-1 size-5 shrink-0 accent-indigo-400 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-indigo-400';
  return (
    <Popover id="viewer-settings" role="dialog" aria-label="Settings" onClose={onClose}
      onKeyDown={keyDown}
      className="right-2 max-h-[calc(100dvh-4rem)] w-80 max-w-[calc(100vw-1rem)] overflow-y-auto p-3 sm:right-3">
      <h2 className="mb-2 px-2 text-sm font-semibold text-zinc-100">Overlays</h2>
      <label className={label}>
        <input ref={first} type="checkbox" aria-label="Window borders" checked={borders} onChange={event => onBorders(event.target.checked)} className={checkbox} aria-describedby="borders-description" />
        <span><span className="block text-sm text-zinc-100">Window borders</span>
          <span id="borders-description" className="mt-1 block text-xs text-zinc-400">Coloured viewer outlines around remote windows. Application title bars and decorations stay unchanged.</span>
        </span>
      </label>
      <label className={label}>
        <input type="checkbox" aria-label="UI elements" checked={elements} onChange={event => onElements(event.target.checked)} className={checkbox} aria-describedby="elements-description" />
        <span><span className="block text-sm text-zinc-100">UI elements</span>
          <span id="elements-description" className="mt-1 block text-xs text-zinc-400">Accessibility outlines for the focused window. Requires server accessibility support and an application tree; this switch cannot enable server support.</span>
        </span>
      </label>
    </Popover>
  );
}
