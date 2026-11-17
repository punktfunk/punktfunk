// GET/PUT /api/plugin-config/<id> — a plugin's `__config`, readable from the CONSOLE origin.
//
// The Library section's "Game sources" settings drawer renders a form from a library plugin's
// `__config` (the kit's generic settings surface, so a scanner needs no SPA of its own).
// `middleware/auth.ts` answers 404 for `/plugin-ui/**` on the console origin, unconditionally and
// by design, so the drawer cannot fetch `/plugin-ui/<id>/__config` itself.
//
// Pointing the drawer at the plugin origin is not the answer. That needs CORS plus cross-site
// cookies, and it would put a plugin-controlled response inside a credentialed cross-origin
// fetch — reopening the hole the split exists to close. What the drawer needs is DATA, not an
// embedded UI: this reads the JSON server-side over loopback and returns it same-origin, so no
// plugin HTML or JS is ever served from the console origin.
//
// Auth: `/api/**` is always session-gated (`isPublicPath`), so reaching here means a logged-in
// operator, and it answers 401 as JSON rather than redirecting — which is what a `fetch` needs. The
// plugin's per-boot secret stays server-side, exactly as in the `/plugin-ui` proxy.
import { defineEventHandler, getRouterParam, setResponseStatus } from "h3";
import { PLUGIN_ID_RE } from "../../../util/pluginProxy";
import { pluginSurface } from "../../../util/pluginSurface";

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id");
	// 400, not 404: the console reads a 404 from here as "this plugin serves no `__config`, its
	// settings are its own page". `PLUGIN_ID_RE` is deliberately narrower than the host's provider
	// rule (which allows `.` and `_`), so a listed source can land here with an id we won't dial —
	// and saying "no settings surface" about it would be a lie.
	if (!id || !PLUGIN_ID_RE.test(id)) {
		setResponseStatus(event, 400);
		return { error: "not a valid plugin id" };
	}
	// `GET` reads schema + current value; `PUT` validates and saves. A 404 is the plugin declining
	// to have a `__config` at all (`config` is optional on the kit's `serveUi`), which the console
	// renders as "settings live on this plugin's own page".
	return pluginSurface(event, id, "/__config", {
		methods: ["GET", "PUT"],
		grantForm: "config",
		notFound: { error: "plugin serves no config surface", noConfig: true },
	});
});
