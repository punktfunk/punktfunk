// Whose page it is. The library shows any profile's view: the box's routes with `?as=<id>`.
// Plugins and Automation read and write a seat's own host through the box
// (`/api/v1/profiles/{id}/proxy/…`). The rest stay the box's.
import { QueryClient } from "@tanstack/react-query";
import { createContext, useContext } from "react";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import { apiErrorCode } from "@/lib/errors";
import { m } from "@/paraglide/messages";
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

/** The pages that differ per profile. The entry page shares the library's pick. */
export type SeatPage = "library" | "plugins" | "automation";

/** One entry of a page's title pick. */
export interface ScopeChoice {
	/** The profile's id; `null` for the owner, whose entry is the box's own. */
	id: string | null;
	name: string;
	/** The seat whose host answers the page's calls; `null` is this box. */
	seat: SeatChoice | null;
	/** The profile the library's calls name in `as`; absent for the owner's library. */
	as?: string;
	/** The profile can have games of its own (`hasOwnGames`). */
	ownGames?: true;
}

export interface SeatScopeValue {
	page: SeatPage;
	/** What the title offers, the owner first. Fewer than two: the title is plain. */
	choices: ScopeChoice[];
	/** The pick the page shows; `null` only when the host has no profiles. */
	current: ScopeChoice | null;
	/** The owner's profile id, which is their seat's on a door. */
	ownerId?: string;
	/** The box is a door: it has no library of its own, so every call goes to a seat. */
	door?: boolean;
	/** The person's own console: their profile is the one choice, and the server picks the host. */
	person?: true;
	/** In a person's console, the owner's name: they add the shared games. */
	owner?: string;
	pick: (id: string | null) => void;
}

/** Page `page` in the person's own console: their profile, viewed as itself, with no pick. */
export function personScope(
	page: SeatPage,
	me: { profile: string; name: string; owner: string | null },
): SeatScopeValue {
	const choice: ScopeChoice = {
		id: me.profile,
		name: me.name,
		seat: null,
		as: me.profile,
		ownGames: true,
	};
	return {
		page,
		choices: [choice],
		current: choice,
		person: true,
		owner: me.owner ?? m.seat_box(),
		pick: () => {},
	};
}

export const SeatScopeContext = createContext<SeatScopeValue | null>(null);

/** The seat the page in view shows; `null` is the box's own, and outside the scoped pages. */
export const useSeat = (): SeatChoice | null =>
	useContext(SeatScopeContext)?.current?.seat ?? null;

/** Whose own account the page's seat runs on, if it is a person's; `null` for every other host. */
export const useOwnAccount = (): OwnAccount | null => useSeat()?.own ?? null;

/**
 * True when the page's plugins are the ones the console reaches: the box's own, and behind a door
 * the owner's, whose host the door asks for them. Another seat's plugins are out of its reach.
 */
export const usePluginsReachable = (): boolean => {
	const scope = useContext(SeatScopeContext);
	if (scope?.person) return false;
	const seat = scope?.current?.seat;
	return !seat || (scope?.door === true && seat.id === scope.ownerId);
};

/** A profile's view of the library: whose it is, and the owner's library it is built on. */
export interface ProfileView {
	id: string;
	name: string;
	ownerName: string;
	/** Games only this profile has are added here, and run on its own desktop. */
	ownGames: boolean;
	/** Switches the console's pick to the owner. */
	toOwner: () => void;
	/** The person themselves, in their own console: there is no owner's library to switch to. */
	person: boolean;
}

/** A profile's marks in the library: by name to the operator, as "you" to the person themselves. */
export interface Whose {
	own: string;
	hidden: string;
	hide: string;
	show: string;
	sourceOff: string;
	library: string;
}

export const whose = (name: string, you: boolean): Whose =>
	you
		? {
				own: m.person_view_own(),
				hidden: m.person_view_hidden(),
				hide: m.person_view_hide(),
				show: m.person_view_show(),
				sourceOff: m.person_view_source_off(),
				library: m.person_library_back(),
			}
		: {
				own: m.profile_view_own({ name }),
				hidden: m.profile_view_hidden_badge({ name }),
				hide: m.profile_view_hide({ name }),
				show: m.profile_view_show({ name }),
				sourceOff: m.profile_view_source_off_own({ name }),
				library: m.seat_library_of({ name }),
			};

/** `who`'s marks as the page's reader says them; `null` without a profile. */
export const useWhose = (who: string | undefined): Whose | null => {
	const you = useContext(SeatScopeContext)?.person === true;
	return who === undefined ? null : whose(who, you);
};

/** The profile whose view of the library the page shows; `null` for the owner's own. */
export const useProfileView = (): ProfileView | null => {
	const scope = useContext(SeatScopeContext);
	const as = scope?.current?.as;
	if (!scope || !as) return null;
	return {
		id: as,
		name: scope.current?.name ?? "",
		ownerName: scope.owner ?? scope.choices[0]?.name ?? m.seat_box(),
		ownGames: scope.current?.ownGames === true,
		toOwner: () => scope.pick(null),
		person: scope.person === true,
	};
};

/** `{ as }` for a library call in a profile's view; `undefined` for the owner's library. */
export const useAs = (): { as: string } | undefined => {
	const as = useContext(SeatScopeContext)?.current?.as;
	return as ? { as } : undefined;
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

/**
 * True for a profile on a desktop the box made for it: a game only it has runs as that account.
 * A person's own account, a shared desktop and taking turns have no games of their own.
 */
export function hasOwnGames(
	p: ProfileAdmin,
	os: string | undefined,
	door = false,
): boolean {
	return (
		!p.owner &&
		isFullSeat(p, os, door) &&
		p.seat?.kind === "desktop" &&
		!p.seat.account
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

/**
 * What page `page`'s title offers, the owner first. The library offers every profile, each
 * read from the owner's host with `as`, and marks those that can have games of their own.
 * Plugins and Automation offer the profiles whose seat has a host of its own. A door's owner is
 * a seat, except on Automation, whose calls the door forwards to the owner itself.
 */
export function scopeChoices(
	page: SeatPage,
	profiles: ProfileAdmin[] | undefined,
	os: string | undefined,
	door = false,
): ScopeChoice[] {
	const list = profiles ?? [];
	const owner = list.find((p) => p.owner);
	const name = owner?.display_name ?? m.seat_box();
	if (page === "library") {
		if (door && !owner) return [];
		const seat = door && owner ? { id: owner.id, name } : null;
		return [
			{ id: null, name, seat },
			...list
				.filter((p) => !p.owner)
				.map((p) => ({
					id: p.id,
					name: p.display_name,
					seat,
					as: p.id,
					...(hasOwnGames(p, os, door) ? { ownGames: true as const } : {}),
				})),
		];
	}
	const seats = seatChoices(list, os, door);
	const ownerSeat =
		door && page === "plugins"
			? (seats.find((s) => s.id === owner?.id) ?? null)
			: null;
	return [
		{ id: null, name, seat: ownerSeat },
		...seats
			.filter((s) => s.id !== owner?.id)
			.map((s) => ({ id: s.id, name: s.name, seat: s })),
	];
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
 * Calls `onGone` with the refusal when a read (`read`) or a write of `client` names a profile
 * the host no longer has. Returns the unsubscribe.
 */
export function onProfileGone(
	client: QueryClient,
	onGone: (error: unknown, read: boolean) => void,
): () => void {
	const gone = (e: unknown) => apiErrorCode(e) === "profile_unknown";
	const reads = client.getQueryCache().subscribe((ev) => {
		if (
			ev.type === "updated" &&
			ev.action.type === "error" &&
			gone(ev.action.error)
		)
			onGone(ev.action.error, true);
	});
	const writes = client.getMutationCache().subscribe((ev) => {
		if (
			ev.type === "updated" &&
			ev.action.type === "error" &&
			gone(ev.action.error)
		)
			onGone(ev.action.error, false);
	});
	return () => {
		reads();
		writes();
	};
}

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
