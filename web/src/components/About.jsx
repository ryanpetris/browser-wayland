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
      <p>A Wayland desktop in your browser.</p>
      <a className={link} href="https://github.com/ryanpetris/browser-wayland" target="_blank" rel="noreferrer">Project and documentation</a>
      <h3 className="font-medium text-zinc-100">Licenses &amp; source</h3>
      <p>Original browser-wayland code is MIT licensed. This software comes without warranty. License terms describe your rights to use, modify and redistribute it.</p>
      <a className={link} href="/assets/license-notices.txt" target="_blank" rel="noreferrer">License notices</a>
      {__BW_VISUALISER__ && <>
        <p>The audio visualiser uses audioMotion-analyzer 4.5.4, licensed under AGPL-3.0-or-later.</p>
        <a className={link} href="/assets/audiomotion-LICENSE.txt" target="_blank" rel="noreferrer">audioMotion license</a>
        <a className={link} href="/assets/audiomotion-source.js" download>audioMotion source</a>
        <a className={link} href="/assets/viewer-source.tar.gz" download>Corresponding viewer source</a>
      </>}
    </Popover>
  );
}
