// What a person's console reaches, as one table. `seat` rows go to the person's own seat host with
// their session token; `view` rows go to the box with the person headers and `as` naming their
// profile. Any call not in the table is refused by this server. personRoutes.test.ts pins each
// row against `x-scope` in api/openapi.json and the host's own lists.

export type Lane = "seat" | "view";
type Method = "GET" | "POST" | "PUT" | "DELETE";

export interface PersonRoute {
	method: Method;
	/** The path after `/api/v1/`, spelled as `api/openapi.json` spells it. */
	pattern: string;
	lane: Lane;
	/** On a `view` row: the calls whose answer only the person's own seat host has. */
	seatWhen?: (params: Record<string, string>) => boolean;
}

const seat = (method: Method, pattern: string): PersonRoute => ({
	method,
	pattern,
	lane: "seat",
});
const view = (method: Method, pattern: string): PersonRoute => ({
	method,
	pattern,
	lane: "view",
});

/** A title only the person has: its seat host minted the id. */
const OWN_ID = /^own:[0-9a-f]{12}$/;

export const PERSON_ROUTES: readonly PersonRoute[] = [
	seat("GET", "host"),
	seat("GET", "hooks"),
	seat("PUT", "hooks"),
	seat("POST", "library/custom"),
	seat("GET", "library/custom/{id}"),
	seat("PUT", "library/custom/{id}"),
	seat("DELETE", "library/custom/{id}"),
	// Their own emulators and plugins, on their seat host. A person's own account refuses all but
	// an adopt there, in the host's words.
	seat("GET", "emulators"),
	seat("POST", "emulators/{id}/adopt"),
	seat("POST", "emulators/{id}/install"),
	seat("POST", "emulators/{id}/remove"),
	seat("GET", "plugins"),
	seat("GET", "store/catalog"),
	seat("POST", "store/refresh"),
	seat("GET", "store/installed"),
	seat("POST", "store/install"),
	seat("POST", "store/uninstall"),
	seat("GET", "store/jobs"),
	seat("GET", "store/jobs/{id}"),
	seat("GET", "store/runtime"),
	seat("POST", "store/runtime"),
	// Folders for those plugins: only the seat's own plugins ask its host, never the box's.
	seat("GET", "plugin-access"),
	seat("POST", "plugin-access/{plugin}/decide"),
	seat("POST", "plugin-access/{plugin}/release"),
	view("GET", "library"),
	view("GET", "library/page"),
	{
		...view("GET", "library/art/{id}/{kind}"),
		// A cover on the person's own desktop, which the box never learns.
		seatWhen: (p) => OWN_ID.test(p.id ?? ""),
	},
	view("GET", "library/scanners"),
	view("PUT", "library/scanners/{id}"),
	view("GET", "library/overrides"),
	view("PUT", "library/overrides/games/{id}"),
	view("PUT", "library/overrides/sources/{source}"),
	view("PUT", "library/hidden/{id}"),
	view("GET", "session/settings"),
	view("PUT", "session/settings"),
	view("DELETE", "session/settings"),
];

/** The route params `rest` gives `pattern`, or `null` when it is another path. */
function match(pattern: string, rest: string): Record<string, string> | null {
	const want = pattern.split("/");
	const got = rest.split("/");
	if (want.length !== got.length) return null;
	const params: Record<string, string> = {};
	for (const [i, segment] of want.entries()) {
		const value = got[i] ?? "";
		const name = /^\{(\w+)\}$/.exec(segment)?.[1];
		if (name && value) params[name] = value;
		else if (segment !== value) return null;
	}
	return params;
}

/** Where `method` `rest` goes for a person, or `null` when their console does not reach it. */
export function personLane(method: string, rest: string): Lane | null {
	const verb = method.toUpperCase() === "HEAD" ? "GET" : method.toUpperCase();
	for (const route of PERSON_ROUTES) {
		if (route.method !== verb) continue;
		const params = match(route.pattern, rest);
		if (!params) continue;
		return route.seatWhen?.(params) ? "seat" : route.lane;
	}
	return null;
}
