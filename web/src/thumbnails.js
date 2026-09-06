// One queued job and one trailing update per row. The shared queue serializes actual requests.
export function thumbnailScheduler({ queue, capture, publish, allowed = () => true, now = () => performance.now(), delay = setTimeout, cancel = clearTimeout }) {
  let current, captured, pending = false, disposed = false, timer, request;
  let lastStart = -Infinity, failures = 0, generation = 0;
  const key = state => JSON.stringify([state.revision, state.sizing]);
  const needed = () => !disposed && current?.eligible && captured !== key(current) && failures < 2 && allowed();
  const pump = () => {
    if (!needed() || pending || timer != null) return;
    const remaining = lastStart + 3000 - now();
    if (remaining > 0) {
      timer = delay(() => { timer = null; pump(); }, remaining);
      return;
    }
    pending = true;
    queue(async () => {
      // Recheck all start conditions after waiting behind other rows.
      if (!needed() || now() < lastStart + 3000) return;
      const selected = current, selectedKey = key(selected), epoch = generation;
      lastStart = now();
      request = new AbortController();
      try {
        const blob = await capture(selected.sizing, request.signal);
        if (disposed || epoch !== generation || !current.eligible) return;
        captured = selectedKey;
        failures = 0;
        publish(blob);
      } catch {
        if (!disposed && epoch === generation && selectedKey === key(current)) failures++;
      } finally { request = null; }
    }).finally(() => { pending = false; pump(); });
  };
  return {
    update(next) {
      if (disposed) return;
      if (!current || key(next) !== key(current) || (!current.eligible && next.eligible)) failures = 0;
      if (!next.eligible || (current && JSON.stringify(next.sizing) !== JSON.stringify(current.sizing))) {
        generation++;
        request?.abort();
        if (timer != null) { cancel(timer); timer = null; }
      }
      current = next;
      pump();
    },
    dispose() {
      disposed = true;
      generation++;
      if (timer != null) cancel(timer);
      request?.abort();
    },
  };
}
