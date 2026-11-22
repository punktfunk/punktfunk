// What a sync of this device's library into Steam does, as data: which titles are wanted, which
// to add, which to remove, the collection each is filed in, and the map sent back to the host.
// Pure, so scripts/test-local-sync.ts runs it under plain node; local-sync.ts carries it out.
import type { LocalEntriesResult, LocalEntry, LocalSource, SteamMap } from "../backend";

/** The host's answer, read: only when it answered and Steam owns the desk. */
export interface LocalAnswer {
  sources: LocalSource[];
  entries: LocalEntry[];
}

/**
 * What a run may remove. `none` at plugin load. `off`, from the panel: entries the host still
 * lists under a source the player switched off. `all`, the panel's Remove: those, and entries
 * whose title left the host's library.
 */
export type Removal = "none" | "off" | "all";

const text = (v: unknown): v is string => typeof v === "string" && v.trim() !== "";

/** The answer, or null when the host did not answer or Steam does not own the desk. An entry
 *  without an id, a title or a source is dropped, as is a source without an id. */
export function readAnswer(r: LocalEntriesResult | null): LocalAnswer | null {
  if (!r?.ok || r.steam_owns_desk !== true) {
    return null;
  }
  return {
    sources: (r.sources ?? []).filter((s) => text(s?.id)).map((s) => ({ ...s, name: text(s.name) ? s.name : s.id })),
    entries: (r.entries ?? [])
      .filter((e) => text(e?.id) && text(e.title) && text(e.source))
      .map((e) => ({ ...e, art: Array.isArray(e.art) ? e.art.filter(text) : [] })),
  };
}

/** The titles of the sources switched on. */
export function wanted(answer: LocalAnswer, on: ReadonlySet<string>): LocalEntry[] {
  return answer.entries.filter((e) => on.has(e.source));
}

export interface Plan {
  /** Wanted, with no entry on record or one Steam does not say is alive. */
  add: LocalEntry[];
  /** Wanted, with a live entry: renamed, re-dressed and filed as needed. */
  keep: Array<[LocalEntry, number]>;
  /** Entries on record to remove, as `[library id, appId]`. */
  drop: Array<[string, number]>;
}

/** The run's work. `alive` is Steam's word on an appId: true, false, or null for can't tell. */
export function plan(
  answer: LocalAnswer,
  on: ReadonlySet<string>,
  have: ReadonlyArray<[string, number]>,
  alive: (appId: number) => boolean | null,
  removal: Removal,
): Plan {
  const recorded = new Map(have);
  const add: LocalEntry[] = [];
  const keep: Array<[LocalEntry, number]> = [];
  for (const e of wanted(answer, on)) {
    const appId = recorded.get(e.id);
    if (appId != null && alive(appId) === true) {
      keep.push([e, appId]);
    } else {
      add.push(e);
    }
  }
  const listed = new Map(answer.entries.map((e) => [e.id, e]));
  const drop = have.filter(([id]) => {
    const e = listed.get(id);
    if (!e) {
      return removal === "all";
    }
    return removal !== "none" && !on.has(e.source);
  });
  return { add, keep, drop };
}

/** Entries on record whose title the host no longer lists: what the panel's Remove takes. */
export function goneIds(answer: LocalAnswer, have: ReadonlyArray<[string, number]>): string[] {
  const listed = new Set(answer.entries.map((e) => e.id));
  return have.map(([id]) => id).filter((id) => !listed.has(id));
}

/** The collection a title is filed in: its platform, else its source's name. */
export function collectionFor(entry: LocalEntry, sources: LocalSource[]): string {
  if (text(entry.platform)) {
    return entry.platform.trim();
  }
  return sources.find((s) => s.id === entry.source)?.name ?? entry.source;
}

/** The wanted titles whose name already carries the stream's controller layout: Steam files a
 *  layout under the lowercase name, so the synced entry shares it. `layoutNames` are lowercase. */
export function sharedLayout(entries: LocalEntry[], layoutNames: ReadonlySet<string>): string[] {
  return [...new Set(entries.map((e) => e.title))].filter((t) => layoutNames.has(t.toLowerCase()));
}

/** The map for the host: every synced entry on record, and every other entry the plugin made. */
export function steamMap(have: ReadonlyArray<[string, number]>, own: Array<number | null>): SteamMap {
  return {
    version: 1,
    entries: Object.fromEntries(have),
    own: [...new Set(own.filter((n): n is number => n != null && n > 0))],
  };
}
