// A seat's own library, game sources and plugins: the three pages that differ per seat read and
// write the seat's host through the box (`/api/v1/profiles/{id}/proxy/…`), the rest stay the box's.
import { QueryClient } from "@tanstack/react-query";
import { createContext, useContext } from "react";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import { inSeat } from "./fetcher";

/** A seat that runs on a person's own account, by the host's platform. Its host refuses hooks; a
 * Linux one also every plugin and emulator change, a Windows one unreviewed plugins and emulator
 * copies added by hand. */
export type OwnAccount = "linux" | "windows";

/** A profile whose seat has a host of its own, so it has a library of its own. */
export interface SeatChoice {
	id: string;
	name: string;
	/** Set when the seat runs on a person's own account. */
	own?: OwnAccount;
}

/** The pages that differ per seat. The entry page shares the library's pick. */
export type SeatPage = "library" | "plugins";

export interface SeatScopeValue {
	page: SeatPage;
	/** The seat the page shows; `null` is the box's own. */
	seat: SeatChoice | null;
	seats: SeatChoice[];
	ownerName: string;
	/** The owner's profile id, which is their seat's on a door. */
	ownerId?: string;
	/** The box is a door: it has no library of its own, so every pick is a seat. */
	door?: boolean;
	pick: (id: string | null) => void;
}

export const SeatScopeContext = createContext<SeatScopeValue | null>(null);

/** The seat the page in view shows; `null` is the box's own, and outside the three pages. */
export const useSeat = (): SeatChoice | null =>
	useContext(SeatScopeContext)?.seat ?? null;

/** Whose own account the page's seat runs on, if it is a person's; `null` for every other host. */
export const useOwnAccount = (): OwnAccount | null => useSeat()?.own ?? null;

/**
 * True when the page's plugins are the ones the console reaches: the box's own, and behind a door
 * the owner's, whose host the door asks for them. Another seat's plugins are out of its reach.
 */
export const usePluginsReachable = (): boolean => {
	const scope = useContext(SeatScopeContext);
	return (
		!scope?.seat || (scope.door === true && scope.seat.id === scope.ownerId)
	);
};

/**
 * True for a profile whose desktop runs its own host. On a door every profile plays on a seat, and
 * the owner's seat is one of those hosts: the owner and every seat of a desktop of its own count.
 * A profile that shares the owner's desktop plays in the owner's host, so it has no library of
 * its own to choose.
 */
export function isFullSeat(
	p: ProfileAdmin,
	os: string | undefined,
	door = false,
): boolean {
	if (door) return p.owner || p.seat?.kind === "desktop";
	// A take-turns profile rides the one console, so it has no host of its own to page into.
	return (
		!p.owner &&
		p.seat != null &&
		!p.seat.take_turns &&
		(os?.startsWith("windows") ?? false)
	);
}

/** The seats with a host of its own. The console reads an account only on a seat that runs on
 * a person's own account, and full seats off a door are Windows ones. */
export function seatChoices(
	profiles: ProfileAdmin[] | undefined,
	os: string | undefined,
	door = false,
): SeatChoice[] {
	const own: OwnAccount = door ? "linux" : "windows";
	return (profiles ?? [])
		.filter((p) => isFullSeat(p, os, door))
		.map((p) => ({
			id: p.id,
			name: p.display_name,
			...(p.seat?.account ? { own } : {}),
		}));
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

/** The cache of seat `id`, when a page has opened it; a seat nobody looked at holds nothing. */
export const seatClientIfAny = (id: string): QueryClient | undefined =>
	clients.get(id);

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
