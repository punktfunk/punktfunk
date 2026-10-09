// `/api/v1/profiles/<id>/proxy/<rest>` reaches a seat's own host through the box. The box forwards
// it with the admin bearer, so the routes that ask for the console password on the box's own host
// ask on the seat's too: `gatedRoute` names the one that guards `rest`.
import { normalizePath } from "./paths";

/** A profile id as the host accepts one: ASCII letters and digits, at most 64. */
const ID = /^[A-Za-z0-9]{1,64}$/;

export interface SeatCall {
	id: string;
	/** The path on the seat's host after `/api/v1/`, dots and escapes resolved. */
	rest: string;
}

/**
 * The seat call `pathname` makes once its dots and escapes are resolved, or `null` when it points
 * anywhere else. The gate and the forward both read the resolved path, so they cannot disagree
 * about it.
 */
export function seatCall(pathname: string): SeatCall | null {
	const m = /^\/api\/v1\/profiles\/([^/]+)\/proxy\/(.+)$/.exec(
		normalizePath(pathname),
	);
	if (!m?.[1] || !m[2] || !ID.test(m[1])) return null;
	return { id: m[1], rest: m[2] };
}

/** `rest`, each segment escaped, as the tail of a seat path. */
export function encodeRest(rest: string): string {
	return rest.split("/").map(encodeURIComponent).join("/");
}

/** The box's own `/api/v1/profiles/<id>/proxy/<tail>`; `tail` is already escaped. */
export function seatPath(id: string, tail: string): string {
	return `/api/v1/profiles/${id}/proxy/${tail}`;
}

/** A plugin UI's credential never reaches a browser, from a seat's host either. */
export function isUiCredential(rest: string): boolean {
	return /^plugins\/[^/]+\/ui-credential$/i.test(rest);
}

/** The files under `routes/api/v1/` whose password gate a seat call goes through (`GATES`). */
export const GATED_ROUTES = [
	"hooks.put",
	"store/install.post",
	"store/sources/[name].put",
	"library/custom.post",
	"library/custom/[id].put",
	"library/provider/[provider].put",
] as const;

/**
 * First segments a seat's host never relays: `proxy_refuses` in
 * `crates/punktfunk-host/src/mgmt/profiles.rs`. A gated route under one needs no `GATES` entry.
 */
export const HOST_REFUSED: readonly string[] = [
	"native",
	"pair",
	"profiles",
	"update",
	"actions",
	"clients",
];

/** The file under `routes/api/v1/` that gates a call, and the route params it reads. */
export interface GatedRoute {
	route: (typeof GATED_ROUTES)[number];
	params: Record<string, string>;
}

/** The box's own password-gated route that guards `method` `rest`, if any. */
export function gatedRoute(method: string, rest: string): GatedRoute | null {
	const [a, b, c, ...more] = rest.split("/");
	const verb = method.toUpperCase();
	if (more.length > 0) return null;
	if (verb === "PUT" && a === "hooks" && !b)
		return { route: "hooks.put", params: {} };
	if (verb === "POST" && a === "store" && b === "install" && !c)
		return { route: "store/install.post", params: {} };
	if (verb === "PUT" && a === "store" && b === "sources" && c)
		return { route: "store/sources/[name].put", params: { name: c } };
	if (a !== "library") return null;
	if (verb === "POST" && b === "custom" && !c)
		return { route: "library/custom.post", params: {} };
	if (verb === "PUT" && b === "custom" && c)
		return { route: "library/custom/[id].put", params: { id: c } };
	if (verb === "PUT" && b === "provider" && c)
		return {
			route: "library/provider/[provider].put",
			params: { provider: c },
		};
	return null;
}
