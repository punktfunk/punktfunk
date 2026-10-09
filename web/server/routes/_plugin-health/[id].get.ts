// GET /_plugin-health/<id> — is this plugin's UI actually up?
//
// The console needs this to decide between mounting the iframe and showing the offline card. The
// browser cannot read the plugin origin without CORS, so the console's own origin probes it
// server-side, through the same dial as the proxy (and its retry on a rotated secret).
//
// Session-gated like every other console route (it is not under a public prefix), so an
// unauthenticated LAN peer cannot enumerate which plugins are running.
import { defineEventHandler, getRouterParam, setResponseStatus } from "h3";
import { callPlugin, PLUGIN_ID_RE } from "../../util/pluginProxy";

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id") ?? "";
	if (!PLUGIN_ID_RE.test(id)) {
		setResponseStatus(event, 400);
		return { ok: false, error: "not a valid plugin id" };
	}
	const resp = await callPlugin(id, "/__health", {
		method: "GET",
		redirect: "manual",
	});
	if (!resp?.ok) {
		setResponseStatus(event, 502);
		return {
			ok: false,
			error: resp ? `health ${resp.status}` : `plugin "${id}" is not running`,
		};
	}
	return { ok: true };
});
