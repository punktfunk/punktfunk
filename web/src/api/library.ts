import { useQueries } from "@tanstack/react-query";
import {
	getGetLibraryPageQueryKey,
	getLibraryPage,
} from "@/api/gen/library/library";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";

/**
 * Library entries by id, for a page that names a handful of games: one single-entry query each,
 * since a library runs to thousands. Returns the lookup; an id still loading or unknown gives
 * undefined.
 */
export function useLibraryEntries(
	ids: readonly (string | null | undefined)[],
): (id: string | null | undefined) => OperatorGameEntry | undefined {
	const unique = [...new Set(ids.filter((id): id is string => !!id))].sort();
	const pages = useQueries({
		queries: unique.map((id) => ({
			queryKey: getGetLibraryPageQueryKey({ id, limit: 1 }),
			queryFn: ({ signal }: { signal: AbortSignal }) =>
				getLibraryPage({ id, limit: 1 }, { signal }),
			staleTime: 5 * 60_000,
		})),
	});
	const items = pages.flatMap((r) => r.data?.items ?? []);
	return (id) => (id ? items.find((x) => x.id === id) : undefined);
}
