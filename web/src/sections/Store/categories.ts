import type { CatalogEntry } from "@/api/gen/model";

/** The store's groups, in page order. An entry naming none of them lands in `other`. */
export const GROUPS = ["library", "metadata", "tools", "other"] as const;
export type Group = (typeof GROUPS)[number];

export const groupOf = (entry: CatalogEntry): Group =>
	GROUPS.find((g) => entry.categories?.includes(g)) ?? "other";

/**
 * The non-empty groups in page order. Inside one, a launcher found on this host comes first, then
 * by title in `locale`'s order. `detected` is tri-state; only a positive probe lifts an entry.
 */
export function groupCatalog(
	entries: CatalogEntry[],
	locale: string,
): [Group, CatalogEntry[]][] {
	const sorted = [...entries].sort(
		(a, b) =>
			Number(b.detected === true) - Number(a.detected === true) ||
			a.title.localeCompare(b.title, locale),
	);
	return GROUPS.map(
		(g) =>
			[g, sorted.filter((e) => groupOf(e) === g)] as [Group, CatalogEntry[]],
	).filter(([, list]) => list.length > 0);
}
