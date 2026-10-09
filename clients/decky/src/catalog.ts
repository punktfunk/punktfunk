// Which paired hosts have which Steam titles — what the Play-from menu on a game page lists.
//
// The source is `punktfunk library <host> --json`, asked of every paired host that is
// answering. Each answer is reduced to the set of Steam appids (the `steam:<appid>` ids; the
// other stores have no Steam page to appear on) and cached per host record in
// localStorage, so a host that is asleep right now still offers the titles it had — the launch
// wakes it. A host the Deck can no longer see, or that no longer trusts it, loses its entry.
import { toaster } from "@decky/api";
import { library } from "./backend";
import type { HostView } from "./hosts";

export interface LibrarySnapshot {
  /** When it was fetched (ms since epoch). */
  at: number;
  steamAppIds: number[];
}

const KEY_PREFIX = "punktfunk:library:";
const snapshots = new Map<string, LibrarySnapshot>();
let hydrated = false;
const listeners = new Set<() => void>();

/** Load what an earlier session cached. Once; storage that throws leaves a memory-only map. */
function hydrate(): void {
  if (hydrated) {
    return;
  }
  hydrated = true;
  try {
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i);
      if (!key?.startsWith(KEY_PREFIX)) {
        continue;
      }
      const raw = localStorage.getItem(key);
      if (!raw) {
        continue;
      }
      const parsed = JSON.parse(raw) as Partial<LibrarySnapshot>;
      if (typeof parsed.at === "number" && Array.isArray(parsed.steamAppIds)) {
        snapshots.set(key.slice(KEY_PREFIX.length), {
          at: parsed.at,
          steamAppIds: parsed.steamAppIds.filter((n): n is number => typeof n === "number"),
        });
      }
    }
  } catch {
    /* storage unavailable or a corrupt entry — the next refresh rebuilds it */
  }
}

function persist(ref: string, snap: LibrarySnapshot | null): void {
  try {
    if (snap) {
      localStorage.setItem(KEY_PREFIX + ref, JSON.stringify(snap));
    } else {
      localStorage.removeItem(KEY_PREFIX + ref);
    }
  } catch {
    /* ignore */
  }
}

function notify(): void {
  for (const listener of listeners) {
    listener();
  }
}

export function subscribeCatalog(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** `steam:570` → 570. Anything else — another store, a malformed id — is null. */
export function steamAppId(id: string): number | null {
  const m = /^steam:(\d{1,10})$/.exec(id);
  return m ? Number(m[1]) : null;
}

/**
 * Refresh the snapshot of every paired host that is answering. A host that is off keeps the
 * snapshot it had; one that has withdrawn its trust loses it, because listing a host that
 * will refuse the connect is worse than not listing it. Records that are gone are pruned.
 */
export async function refreshLibraries(views: HostView[]): Promise<void> {
  hydrate();
  const live = new Set(views.filter((v) => v.saved).map((v) => v.ref));
  for (const ref of [...snapshots.keys()]) {
    if (!live.has(ref)) {
      snapshots.delete(ref);
      persist(ref, null);
    }
  }
  await Promise.all(
    views
      .filter((v) => v.saved && v.paired && v.online)
      .map(async (v) => {
        const r = await library(v.ref).catch(() => null);
        if (!r) {
          return;
        }
        if (r.ok) {
          const steamAppIds = (r.games ?? [])
            .map((g) => steamAppId(g.id))
            .filter((n): n is number => n != null);
          // The feature is invisible until a game's ▾ menu is opened, so the first time a host's
          // titles arrive, say so — once per host record. A Decky toast body clips past roughly
          // forty characters, so this is a count and a name, nothing more.
          if (!snapshots.has(v.ref) && steamAppIds.length > 0) {
            toaster.toast({
              title: `Stream from ${v.name}`,
              body: `${steamAppIds.length} Steam ${steamAppIds.length === 1 ? "game" : "games"} ready in Play menus`,
              duration: 8_000,
            });
          }
          const snap = { at: Date.now(), steamAppIds };
          snapshots.set(v.ref, snap);
          persist(v.ref, snap);
        } else if (r.error === "needs-pairing" || r.error === "refused") {
          snapshots.delete(v.ref);
          persist(v.ref, null);
        }
        // Anything else (unreachable, a client hiccup) keeps the last good snapshot.
      }),
  );
  notify();
}

/**
 * The hosts that can stream this Steam title, best first: online before asleep, then most
 * recently used. Paired records only — the library came from them, and only they can launch.
 */
export function hostsForApp(appId: number, views: HostView[]): HostView[] {
  hydrate();
  return views
    .filter(
      (v) =>
        v.saved &&
        v.paired &&
        (v.online || v.wakeable) &&
        (snapshots.get(v.ref)?.steamAppIds.includes(appId) ?? false),
    )
    .sort((a, b) => {
      if (a.online !== b.online) {
        return a.online ? -1 : 1;
      }
      return (b.lastUsed ?? 0) - (a.lastUsed ?? 0);
    });
}
