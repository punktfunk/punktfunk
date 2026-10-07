import type { QueryClient } from "@tanstack/react-query";
import {
	getGetLibraryPageQueryKey,
	getGetLibraryQueryKey,
} from "@/api/gen/library/library";
import type { GameEntry } from "@/api/gen/model/gameEntry";
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
