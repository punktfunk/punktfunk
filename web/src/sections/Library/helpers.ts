import type { InfiniteData, QueryClient } from "@tanstack/react-query";
import { seatUrl } from "@/api/fetcher";
import {
	getGetLibraryPageQueryKey,
	getGetLibraryQueryKey,
} from "@/api/gen/library/library";
import type { Artwork } from "@/api/gen/model/artwork";
import type { CustomEntry } from "@/api/gen/model/customEntry";
import type { GameEntry } from "@/api/gen/model/gameEntry";
import type { LibraryPage } from "@/api/gen/model/libraryPage";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import { m } from "@/paraglide/messages";

/** The custom-CRUD path param is the raw id without the `custom:` prefix. */
export function customId(entry: GameEntry): string {
	return entry.id.startsWith("custom:")
		? entry.id.slice("custom:".length)
		: entry.id;
}

/** The operator owns this entry and may edit or delete it. A provider-synced custom entry is
 * refused by the host (409), so it counts as managed. */
export function isOperatorOwned(entry: GameEntry): boolean {
	return entry.store === "custom" && !entry.provider;
}

/**
 * Display label for a store badge. Steam and custom keep their localized strings; any other store
 * shows its source's name (`nameOf`), or its id capitalized where nothing names it.
 */
export function storeLabel(
	store: string,
	nameOf?: (id: string) => string | undefined,
): string {
	switch (store) {
		case "custom":
			return m.library_store_custom();
		case "steam":
			return m.library_store_steam();
		default:
			return nameOf?.(store) ?? store.charAt(0).toUpperCase() + store.slice(1);
	}
}

/** Who an entry is from, as one name: its plugin, else its store. */
export function sourceLabel(
	entry: GameEntry,
	nameOf?: (id: string) => string | undefined,
): string {
	if (entry.provider && entry.provider !== entry.store)
		return nameOf?.(entry.provider) ?? entry.provider;
	return storeLabel(entry.store, nameOf);
}

/** A game only the profile in view has: an id its own desktop minted. */
export const isOwnId = (id: string): boolean => /^own:[0-9a-f]{12}$/.test(id);

const ART_KINDS = ["portrait", "hero", "header", "logo"] as const;

/**
 * Profile `profile`'s own game as its entry page draws it: the box's row once the box lists it,
 * else the stored one. A cover on the profile's desktop, which the box never learns, comes
 * through that desktop.
 */
export function ownEntry(
	listed: OperatorGameEntry | undefined,
	row: CustomEntry,
	profile: string,
): OperatorGameEntry {
	const art: Artwork = { ...listed?.art };
	for (const kind of ART_KINDS) {
		const v = row.art?.[kind]?.trim();
		if (art[kind] || !v) continue;
		art[kind] = /^(https?:|data:)/.test(v)
			? v
			: seatUrl(
					`/api/v1/library/art/${row.id}/${kind}?v=${encodeURIComponent(v)}`,
					profile,
				);
	}
	return { ...(listed ?? { ...row, store: "custom" }), art, own: true };
}

/** A page of the library, or the pages of its endless list. */
type Loaded = LibraryPage | InfiniteData<LibraryPage>;

const isPaged = (d: Loaded): d is InfiniteData<LibraryPage> => "pages" in d;

/**
 * Takes title `id` off every loaded page of the library. The box lists a profile's own game a
 * moment longer than its desktop keeps it; the box's change event refreshes the pages after.
 */
export function dropTitle(qc: QueryClient, id: string): void {
	qc.setQueriesData<Loaded>(
		{ queryKey: getGetLibraryPageQueryKey() },
		(old: Loaded | undefined) => {
			if (!old) return old;
			const pages = isPaged(old) ? old.pages : [old];
			if (!pages.some((p) => p.items.some((g) => g.id === id))) return old;
			const drop = (p: LibraryPage): LibraryPage => ({
				...p,
				items: p.items.filter((g) => g.id !== id),
				total: Math.max(0, p.total - 1),
			});
			return isPaged(old) ? { ...old, pages: old.pages.map(drop) } : drop(old);
		},
	);
}

/**
 * The library changed: every view of it asks again. The whole list and its pages are two
 * queries with two keys, and a change that refreshed one left the other showing the old title.
 */
export function refreshLibrary(qc: QueryClient): Promise<void> {
	return Promise.all([
		qc.invalidateQueries({ queryKey: getGetLibraryQueryKey() }),
		qc.invalidateQueries({ queryKey: getGetLibraryPageQueryKey() }),
	]).then(() => undefined);
}
