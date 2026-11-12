// GET /_plugin-health/<id> — is this plugin's UI up, and at which page path?
//
// The console probes the plugin server-side, through the same dial as the proxy (and its retry on
// a rotated secret), and only then mounts the iframe. A live plugin's answer carries `path`, the
// credential-bearing `/plugin-ui/…/` its frame and "open in new tab" use.
//
// Session-gated like every other console route, and readable only by the console's own origin, so
// neither a LAN peer nor a plugin page can mint a path.
import { defineEventHandler, getRouterParam, setResponseStatus } from "h3";
import { mintPluginPagePath } from "../../util/pluginPage";
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
	return { ok: true, path: mintPluginPagePath(id) };
});
