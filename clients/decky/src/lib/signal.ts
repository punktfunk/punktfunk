// One module-level value that tells its listeners when it changes. The scan store, the library
// catalog and the running set each hold one, so the QAM panel and Steam's game page, which is no
// child of the panel, read the same state.
import { useEffect, useState } from "react";

export interface Signal<T> {
  get(): T;
  /** Store a new value, then tell every listener. */
  set(value: T): void;
  /** Tell every listener that state behind this signal changed in place. */
  notify(): void;
  /** Returns the unsubscribe. */
  subscribe(listener: () => void): () => void;
}

export function signal<T>(initial: T): Signal<T> {
  let value = initial;
  const listeners = new Set<() => void>();
  const notify = (): void => {
    for (const listener of listeners) {
      listener();
    }
  };
  return {
    get: () => value,
    set: (next) => {
      value = next;
      notify();
    },
    notify,
    subscribe: (listener) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
  };
}

/** The signal's value as React state. Re-renders on `set` with a new value; a `notify` of an
 *  in-place change keeps the same value, so React skips it. */
export function useSignal<T>(s: Signal<T>): T {
  const [state, setState] = useState(s.get());
  useEffect(() => {
    const unsubscribe = s.subscribe(() => setState(s.get()));
    setState(s.get()); // a change that landed between render and subscribe
    return unsubscribe;
  }, [s]);
  return state;
}
