// `/api/v1/profiles/<id>/proxy/<rest>` reaches a seat's own host through the box. The box forwards
// it with the admin bearer, so the routes that ask for the console password on the box's own host
// ask on the seat's too: `seatGate` in gates.ts names the one that guards `rest`.
import { normalizeSegments } from "./paths";

/** A profile id as the host accepts one: ASCII letters and digits, at most 64. */
const ID = /^[A-Za-z0-9]{1,64}$/;

export interface SeatCall {
	id: string;
	/** The path on the seat's host after `/api/v1/`, dots and escapes resolved. */
	rest: string;
}

/**
 * The seat call `pathname` makes once its dots and escapes are resolved, or `null` when it points
 * anywhere else. A segment that decodes to hold a `/` (`%2F`) is refused: the forward would emit
 * it as a separator and the host would resolve dots the gate never saw. The gate and the forward
 * both read the returned path, so they cannot disagree about it.
 */
export function seatCall(pathname: string): SeatCall | null {
	const [api, v1, profiles, id, proxy, ...rest] = normalizeSegments(pathname);
	if (
		api !== "api" ||
		v1 !== "v1" ||
		profiles !== "profiles" ||
		proxy !== "proxy"
	)
		return null;
	if (!id || !ID.test(id) || rest.length === 0) return null;
	if (rest.some((s) => /^\.{0,2}$|\//.test(s))) return null;
	return { id, rest: rest.join("/") };
}

/** `rest`, each segment escaped, as the tail of a seat path. */
export function encodeRest(rest: string): string {
	return rest.split("/").map(encodeURIComponent).join("/");
}

/** The box's own `/api/v1/profiles/<id>/proxy/<tail>`; `tail` is already escaped. */
export function seatPath(id: string, tail: string): string {
	return `/api/v1/profiles/${id}/proxy/${tail}`;
}

/**
 * A plugin's UI credential, or its page relay (`ui`, `ui/`, `ui/**`). The console's own
 * `/plugin-ui` proxy calls both server-side; a browser gets neither, from a seat's host either:
 * a plugin's page on the console origin would run as the console.
 */
export function isPluginPrivate(rest: string): boolean {
	return /^plugins\/[^/]+\/(?:ui-credential|ui(?:\/.*)?)$/i.test(rest);
}
