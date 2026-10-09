// /api/** → the management API. By the time we get here the gate (middleware/auth.ts) has
// confirmed an authenticated session. We inject the management bearer token server-side
// (the browser never sees it) and drop the browser's own cookies/auth from the upstream
// request, then proxy. The management API itself binds loopback only — this proxy is the
// ONLY path to it from the LAN, and it's authenticated.
import { defineEventHandler, getRequestURL, setResponseStatus } from "h3";
import { relayToMgmt } from "../../util/mgmt";
import { normalizePath } from "../../util/paths";
import { isUiCredential } from "../../util/seatProxy";

/** `pathname` after `/api/v1/`, one trailing slash dropped, or "" for any other path. */
const apiRest = (pathname: string): string =>
	/^\/api\/v1\/(.*?)\/?$/i.exec(pathname)?.[1] ?? "";

export default defineEventHandler((event) => {
	const { pathname, search } = getRequestURL(event);
	// A plugin UI's proxy credential (its per-boot secret) is fetched server-side by the
	// /plugin-ui proxy and must NEVER reach a browser — deny it on the generic passthrough so a
	// session-authed page can't read it (plugin-ui-surface §5, D6). The secret-free list at
	// /api/v1/plugins is fine; only the {id}/ui-credential leaf is blocked.
	//
	// Matched against the NORMALIZED path as well as the raw one: `/api//v1/...`, `/api/./v1/...`
	// and percent-encoded variants all reach the same upstream route, and a denylist that only
	// knows the canonical spelling is one router-quirk away from leaking the secret.
	if (
		isUiCredential(apiRest(pathname)) ||
		isUiCredential(apiRest(normalizePath(pathname)))
	) {
		setResponseStatus(event, 403);
		return {
			error: "plugin UI credentials are not accessible from the browser",
		};
	}
	return relayToMgmt(event, `${pathname}${search}`);
});
