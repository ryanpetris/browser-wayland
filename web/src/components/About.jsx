import { useEffect, useRef } from 'react';
import { Popover } from './Launcher.jsx';

export function About({ viewer, onClose }) {
  const close = useRef(null);
  useEffect(() => {
    viewer.releaseInput();
    if (document.pointerLockElement) document.exitPointerLock();
    close.current?.focus();
  }, [viewer]);
  const keyDown = event => {
    event.stopPropagation();
    if (event.key === 'Tab') {
      const controls = [...event.currentTarget.querySelectorAll('a, button')];
      const index = controls.indexOf(document.activeElement);
      event.preventDefault();
      controls[index < 0 ? (event.shiftKey ? controls.length - 1 : 0) : (index + (event.shiftKey ? controls.length - 1 : 1)) % controls.length].focus();
    }
  };
  const link = 'w-fit text-indigo-300 underline underline-offset-2 hover:text-indigo-200';
  return (
    <Popover id="viewer-about" role="dialog" aria-label="About browser-wayland" onClose={onClose} onKeyDown={keyDown} onPaste={event => event.stopPropagation()}
      className="right-2 max-h-[calc(100dvh-4rem)] w-96 max-w-[calc(100vw-1rem)] gap-3 overflow-y-auto p-4 text-sm select-text sm:right-3">
      <div className="flex items-center justify-between gap-3">
        <h2 className="font-semibold text-zinc-100">browser-wayland</h2>
        <button ref={close} type="button" onClick={onClose} className="rounded px-2 py-1 hover:bg-zinc-800" aria-label="Close About">Close</button>
      </div>
      <p>Copyright © 2026 Ryan Petris.</p>
      <p>browser-wayland's original code is available under the MIT License. It includes open source software under other licenses and is provided without warranty.</p>
      <a className={link} href="/assets/license-notices.txt" target="_blank" rel="noreferrer">Open source licenses</a>
      <p>You can download the source and find build instructions in the{' '}
        <a className={link} href="https://github.com/ryanpetris/browser-wayland#source-code" target="_blank" rel="noreferrer">GitHub repository</a>.
      </p>
    </Popover>
  );
}
