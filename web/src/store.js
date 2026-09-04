// A tiny external store: the engine (viewer.js) publishes state, React reads it with useSyncExternalStore.
import { useSyncExternalStore } from 'react';

export function createStore(initial) {
  let state = initial;
  const subs = new Set();
  return {
    get: () => state,
    set(patch) {
      state = { ...state, ...patch };
      for (const f of subs) f();
    },
    subscribe(f) {
      subs.add(f);
      return () => subs.delete(f);
    },
  };
}

/// One field (or a selector returning something already stored) of the store, re-rendering when it changes.
export const useStore = (store, select) => useSyncExternalStore(store.subscribe, () => select(store.get()));
