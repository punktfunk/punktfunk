import type { HostInfo } from "@/api/gen/model/hostInfo";
import type { SeatCheck } from "@/api/gen/model/seatCheck";
import type { SeatPublic } from "@/api/gen/model/seatPublic";
import type { UpdateResultInfo } from "@/api/gen/model/updateResultInfo";
import { m } from "@/paraglide/messages";

/** The values of a host message's placeholders, by name. */
type Params = Record<string, string> | null | undefined;

// Every message by key. Looking one up by a key the host sent cannot be tree-shaken, so the
// whole catalogue ships.
const CATALOGUE = m as unknown as Record<
	string,
	(params: Record<string, string>) => string
>;

/**
 * The host message `id` in the console's language, or `undefined` when this build has no such
 * message or `params` lacks a value it needs. A console and a host a version apart disagree on
 * ids, so a miss is normal and the caller shows the host's English.
 */
export function hostMessage(id: string, params?: Params): string | undefined {
	const message = Object.hasOwn(CATALOGUE, id) ? CATALOGUE[id] : undefined;
	if (!message) return undefined;
	let complete = true;
	const given = params ?? {};
	const read = new Proxy(given, {
		get(target, key) {
			if (typeof key !== "string") return undefined;
			if (!Object.hasOwn(target, key)) complete = false;
			return target[key];
		},
	});
	const text = message(read);
	return complete ? text : undefined;
}

/** The host's sentence by message id, else the English it arrived with. */
export function hostText<E extends string | null | undefined>(
	id: string | null | undefined,
	params: Params,
	english: E,
): string | E {
	return (id ? hostMessage(id, params) : undefined) ?? english;
}

/** The sentence for an API error `code`: the host message `apierr_<code>`. */
export function apiText<E extends string | null | undefined>(
	code: string | null | undefined,
	params: Params,
	english: E,
): string | E {
	return hostText(code && `apierr_${code}`, params, english);
}

/** One line of what a seat's doctor found. */
export const seatCheckText = (c: SeatCheck): string =>
	hostText(c.id, c.params, c.message);

/** A seat's state line while it starts or is unavailable. */
export const seatDetail = (s: SeatPublic): string | null | undefined =>
	hostText(s.detail_id, s.detail_params, s.detail);

/** Why the door can't be switched on. */
export const doorUnavailable = (h: HostInfo): string | null | undefined =>
	apiText(
		h.door_unavailable_code,
		h.door_unavailable_params,
		h.door_unavailable,
	);

/** Why the last update failed: the host message `hostupdate_<error_code>`. */
export function updateError(r: UpdateResultInfo): string | null | undefined {
	const params = { ...r.error_params, from: r.from, to: r.to };
	return hostText(
		r.error_code && `hostupdate_${r.error_code}`,
		params,
		r.error,
	);
}
