// Desktop notifications as a stack of toasts over the stage: the application's icon, summary and body,
// its action buttons, and a close button. Clicking the toast is the notification's default action.
import { useEffect, useState } from 'react';
import { Bell, X } from 'lucide-react';
import { useStore } from '../store.js';
import { notificationIcon } from '../api.js';

// Notification bodies may carry a little markup (<b>, <a>); only their text is shown.
const plain = html => new DOMParser().parseFromString(html, 'text/html').body.textContent ?? '';

export function Notifications({ viewer }) {
  const list = useStore(viewer.store, s => s.notifications);
  if (!list.length) return null;
  return (
    <div className="absolute top-3 right-3 flex w-80 max-w-[90%] flex-col gap-2">
      {list.map(n => <Toast key={n.id} n={n} viewer={viewer} />)}
    </div>
  );
}

function Toast({ n, viewer }) {
  const [icon, setIcon] = useState(null);
  useEffect(() => {
    // fetched again when the application replaces the notification (rev); the old picture is released
    let live = true, url = null;
    setIcon(null);
    if (n.icon) notificationIcon(n.id).then(u => { if (live) { url = u; setIcon(u); } else if (u) URL.revokeObjectURL(u); });
    return () => { live = false; if (url) URL.revokeObjectURL(url); };
  }, [n.id, n.rev, n.icon]);
  const act = (e, action) => { e.stopPropagation(); viewer.notify(n.id, action); };
  const buttons = n.actions.filter(([key]) => key !== 'default');
  return (
    <div onClick={e => act(e, 'default')} className="cursor-pointer rounded-lg border border-zinc-700 bg-zinc-900/95 p-3 text-sm text-zinc-200 shadow-lg backdrop-blur">
      <div className="flex items-start gap-2.5">
        {icon ? <img src={icon} alt="" className="mt-0.5 size-8 shrink-0 object-contain" /> : <Bell className="mt-0.5 size-8 shrink-0 p-1.5 text-zinc-500" />}
        <div className="min-w-0 flex-1">
          <div className="flex items-baseline gap-2">
            <span className="truncate font-medium text-zinc-100">{n.summary}</span>
            {n.app && <span className="ml-auto shrink-0 text-[11px] text-zinc-500">{n.app}</span>}
          </div>
          {n.body && <div className="mt-0.5 line-clamp-4 text-xs whitespace-pre-line text-zinc-300">{plain(n.body)}</div>}
          {buttons.length > 0 && (
            <div className="mt-2 flex flex-wrap gap-1.5">
              {buttons.map(([key, label]) => (
                <button key={key} type="button" onClick={e => act(e, key)} className="rounded-md bg-zinc-800 px-2 py-1 text-xs text-zinc-200 hover:bg-zinc-700">{label}</button>
              ))}
            </div>
          )}
        </div>
        <button type="button" onClick={e => act(e, undefined)} title="Dismiss" className="-mt-1 -mr-1 rounded p-1 text-zinc-500 hover:bg-zinc-800 hover:text-zinc-200"><X className="size-4" /></button>
      </div>
    </div>
  );
}
