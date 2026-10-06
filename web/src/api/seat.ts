// A seat's own library, game sources and plugins: the three pages that differ per seat read and
// write the seat's host through the box (`/api/v1/profiles/{id}/proxy/…`), the rest stay the box's.
import { QueryClient } from "@tanstack/react-query";
import { createContext, useContext } from "react";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import { inSeat } from "./fetcher";

/** A profile whose seat has a host of its own, so it has a library of its own. */
export interface SeatChoice {
	id: string;
	name: string;
}

/** The pages that differ per seat. The entry page shares the library's pick. */
export type SeatPage = "library" | "plugins";

export interface SeatScopeValue {
	page: SeatPage;
	/** The seat the page shows; `null` is the box's own. */
	seat: SeatChoice | null;
	seats: SeatChoice[];
	ownerName: string;
	pick: (id: string | null) => void;
}

export const SeatScopeContext = createContext<SeatScopeValue | null>(null);

/** The seat the page in view shows; `null` is the box's own, and outside the three pages. */
export const useSeat = (): SeatChoice | null =>
	useContext(SeatScopeContext)?.seat ?? null;

/** True for a profile whose desktop runs its own host. Linux seats join here. */
export function isFullSeat(p: ProfileAdmin, os: string | undefined): boolean {
	return !p.owner && p.seat != null && (os?.startsWith("windows") ?? false);
}

export function seatChoices(
	profiles: ProfileAdmin[] | undefined,
	os: string | undefined,
): SeatChoice[] {
	return (profiles ?? [])
		.filter((p) => isFullSeat(p, os))
		.map((p) => ({ id: p.id, name: p.display_name }));
}

const bound = new WeakSet<object>();

/** `o` with its `key` function run inside seat `id`. Binding twice is a no-op. */
function bind<O extends object>(
	id: string,
	o: O,
	key: "queryFn" | "mutationFn",
): O {
	const fn = (o as Record<string, unknown>)[key];
	if (typeof fn !== "function" || bound.has(fn)) return o;
	const run = (...args: unknown[]) => inSeat(id, () => fn(...args));
	bound.add(run);
	return { ...o, [key]: run };
}

const clients = new Map<string, QueryClient>();

/**
 * The cache of seat `id`, whose every query and mutation goes to that seat's host.
 *
 * Nothing it holds is the box's and nothing the box holds is a seat's. Kept for the session, so
 * a page revisited is still warm.
 */
export function seatClient(id: string, root: QueryClient): QueryClient {
	let client = clients.get(id);
	if (client) return client;
	client = new QueryClient({ defaultOptions: root.getDefaultOptions() });
	const query = client.defaultQueryOptions.bind(client);
	const mutation = client.defaultMutationOptions.bind(client);
	client.defaultQueryOptions = (o) => bind(id, query(o), "queryFn");
	client.defaultMutationOptions = (o) => bind(id, mutation(o), "mutationFn");
	clients.set(id, client);
	return client;
}
