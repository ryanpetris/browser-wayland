import { useEffect, useRef, useState } from 'react';
import { useStore } from '../store.js';
import { files, deleteFile, downloadFile, manageFile } from '../api.js';

const button = 'rounded bg-zinc-800 px-2 py-1 text-xs hover:bg-zinc-700 disabled:opacity-40';
const input = 'min-w-0 rounded border border-zinc-700 bg-zinc-950 px-2 py-1 text-xs';
const join = (path, name) => `${path === '/' ? '' : path}/${name}`;
const size = n => n < 1024 ? `${n} B` : n < 1048576 ? `${(n / 1024).toFixed(0)} KB` : `${(n / 1048576).toFixed(1)} MB`;

export function FilesPanel({ viewer, open }) {
  const path = useStore(viewer.store, s => s.filesPath);
  const role = useStore(viewer.store, s => s.role);
  const change = useStore(viewer.store, s => s.filesChange);
  const filesOpen = useStore(viewer.store, s => s.filesOpen);
  const upload = useStore(viewer.store, s => s.upload);
  const [started, setStarted] = useState(false);
  const [draft, setDraft] = useState(path);
  const [options, setOptions] = useState({ hidden: false, sort: 'name', desc: false, offset: 0 });
  const [revision, setRevision] = useState(0);
  const [listing, setListing] = useState(null);
  const [error, setError] = useState(null);
  const [loading, setLoading] = useState(false);
  const current = useRef({ path, listing });
  const canonical = useRef(null);
  current.current = { path, listing };
  const acts = ['controller', 'participant'].includes(role);
  useEffect(() => { if (open) setStarted(true); }, [open]);
  useEffect(() => setDraft(path), [path]);
  useEffect(() => { if (filesOpen) setOptions(o => ({ ...o, offset: 0 })); }, [filesOpen]);
  useEffect(() => {
    if (!started || !acts) return;
    // A successful response resolves shortcuts and symlink directories without a second request.
    if (canonical.current?.path === path && canonical.current.revision === revision && canonical.current.options === options) { canonical.current = null; return; }
    const abort = new AbortController();
    setLoading(true); setError(null); setListing(null);
    files({ path, ...options }, abort.signal).then(result => {
      if (abort.signal.aborted) return;
      setListing(result); setLoading(false);
      if (path !== result.path) {
        canonical.current = { path: result.path, revision, options };
        viewer.store.set({ filesPath: result.path });
      }
    }).catch(e => {
      if (!abort.signal.aborted) { setError(e); setLoading(false); }
    });
    return () => abort.abort();
  }, [started, acts, path, options, revision, viewer]);
  useEffect(() => {
    if (change && (change.directories.includes(current.current.path) || change.requested === current.current.path)) setRevision(n => n + 1);
  }, [change]);
  const navigate = next => {
    canonical.current = null;
    setOptions(o => ({ ...o, offset: 0 }));
    viewer.store.set({ filesPath: next });
  };
  const refresh = () => { canonical.current = null; setRevision(n => n + 1); };
  const mutate = async operation => {
    const destination = listing.path;
    try {
      await operation(destination);
      if (current.current.path === destination) refresh();
    } catch (e) { viewer.notice(e.message); }
  };
  if (!acts) return null;
  const parts = (listing?.path || path).split('/').filter(Boolean);
  const errorText = error?.code === 'permission_denied' ? 'Permission denied.' : error?.code === 'missing' ? 'Directory not found.' : 'Could not read this directory.';
  return (
    <section aria-label="File browser" className="flex flex-col gap-2 p-2 text-xs" onFocus={viewer.releaseInput}>
      <div className="flex flex-wrap gap-1">
        <button className={button} onClick={() => navigate('@home')}>Home</button>
        <button className={button} onClick={() => navigate('@transfer')}>Transfer folder</button>
        <button className={button} onClick={refresh}>Refresh</button>
      </div>
      <form className="flex gap-1" onSubmit={e => { e.preventDefault(); navigate(draft); }}>
        <input aria-label="Directory path" className={`${input} flex-1`} value={draft} onChange={e => setDraft(e.target.value)} spellCheck={false} />
        <button className={button}>Go</button>
      </form>
      {listing && <nav aria-label="Directory breadcrumbs" className="flex flex-wrap gap-1 break-all">
        <button className={button} onClick={() => navigate('/')}>/</button>
        {parts.map((part, i) => <button key={i} className={button} onClick={() => navigate('/' + parts.slice(0, i + 1).join('/'))}>{part}</button>)}
        <button className={button} disabled={listing.path === '/'} onClick={() => navigate('/' + parts.slice(0, -1).join('/'))}>Parent</button>
      </nav>}
      <div className="flex flex-wrap items-center gap-2">
        <select aria-label="Sort files" className={input} value={options.sort} onChange={e => setOptions(o => ({ ...o, sort: e.target.value, offset: 0 }))}>
          <option value="name">Name</option><option value="size">Size</option><option value="modified">Modified</option>
        </select>
        <label><input type="checkbox" checked={options.desc} onChange={e => setOptions(o => ({ ...o, desc: e.target.checked, offset: 0 }))} /> Descending</label>
        <label><input type="checkbox" checked={options.hidden} onChange={e => setOptions(o => ({ ...o, hidden: e.target.checked, offset: 0 }))} /> Hidden files</label>
      </div>
      {listing && <div className="flex gap-1">
        <label className={`${button} cursor-pointer`}>Upload<input aria-label="Upload files" type="file" multiple className="hidden" onChange={e => { viewer.uploadFiles(e.target.files); e.target.value = ''; }} /></label>
        <button className={button} onClick={() => { const name = prompt('New directory name'); if (name) mutate(path => manageFile({ op: 'mkdir', path, name })); }}>New folder</button>
      </div>}
      {upload && <div className="break-all" role="status">Uploading {upload.name} ({upload.index}/{upload.count}) to {upload.path} <button className={button} onClick={viewer.cancelUpload}>Cancel</button></div>}
      {loading && <p role="status">Loading directory…</p>}
      {error && <p role="alert">{errorText} {error.message}</p>}
      {listing?.entries.length === 0 && <p>{listing.total ? 'No entries on this page.' : 'This directory is empty.'}</p>}
      {listing?.entries.map(entry => {
        const folder = entry.kind === 'directory' || entry.target_kind === 'directory';
        const downloadable = entry.kind === 'file' || entry.target_kind === 'file';
        return <div key={entry.name} className="border-b border-zinc-800 py-2" data-file-name={entry.name}>
          <div className="break-all text-sm">
            {folder ? <button className="text-indigo-300 hover:underline" onClick={() => navigate(join(listing.path, entry.name))}>{entry.name}/</button> : entry.name}
            {entry.kind === 'symlink' && <span className="ml-1 text-zinc-500">↗ symlink{!entry.target_kind ? ' (unavailable target)' : ''}</span>}
          </div>
          <div className="text-zinc-500">{!folder && `${size(entry.size)} · `}{new Date(entry.modified_ms).toLocaleString()}</div>
          <div className="mt-1 flex gap-1">
            {downloadable && <button className={button} onClick={() => downloadFile(entry.name, listing.path).catch(e => viewer.notice(e.message))}>Download</button>}
            <button className={button} onClick={() => { const new_name = prompt('Rename entry', entry.name); if (new_name && new_name !== entry.name) mutate(path => manageFile({ op: 'rename', path, name: entry.name, new_name })); }}>Rename</button>
            {entry.kind !== 'directory' && <button className={button} onClick={() => { if (confirm(`Delete ${entry.name}?`)) mutate(path => deleteFile(entry.name, path)); }}>Delete</button>}
          </div>
        </div>;
      })}
      {listing && <div className="flex items-center gap-2">
        <button className={button} disabled={!options.offset} onClick={() => setOptions(o => ({ ...o, offset: Math.max(0, o.offset - listing.limit) }))}>Previous</button>
        <span>{listing.total ? `${listing.offset + 1}–${listing.offset + listing.entries.length} of ${listing.total}` : '0 entries'}</span>
        <button className={button} disabled={listing.offset + listing.limit >= listing.total} onClick={() => setOptions(o => ({ ...o, offset: o.offset + listing.limit }))}>Next</button>
      </div>}
      {!!listing?.omitted && <p>{listing.omitted} names omitted because they are not UTF-8.</p>}
    </section>
  );
}
