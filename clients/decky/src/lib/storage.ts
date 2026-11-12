// The plugin's persistence: Steam's CEF localStorage, which outlives a plugin reinstall. Every
// access is guarded, so storage that throws never takes a launch or a render with it: a read
// that throws reads as absent and a write that throws is dropped. The key strings are on
// installed Decks; renaming one orphans that state.

/** The value at `key`; null when it is absent or storage threw. */
export function read(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

/** Store `value` at `key`; null removes the key. */
export function write(key: string, value: string | null): void {
  try {
    if (value == null) {
      localStorage.removeItem(key);
    } else {
      localStorage.setItem(key, value);
    }
  } catch {
    /* storage unavailable */
  }
}

/** The JSON at `key`; null when it is absent, corrupt, or storage threw. */
export function readJson<T>(key: string): T | null {
  const raw = read(key);
  try {
    return raw ? (JSON.parse(raw) as T) : null;
  } catch {
    return null;
  }
}

/** Every key starting with `prefix`, collected before any caller removes one (removing shifts
 *  the indices). Null when storage threw, so a caller can tell "none" from "can't tell". */
export function keys(prefix: string): string[] | null {
  try {
    const found: string[] = [];
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i);
      if (key?.startsWith(prefix)) {
        found.push(key);
      }
    }
    return found;
  } catch {
    return null;
  }
}
