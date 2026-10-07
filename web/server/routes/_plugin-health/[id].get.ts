// GET /_plugin-health/<id> — is this plugin's UI actually up?
//
// The console needs this to decide between mounting the iframe and showing the offline card. It
// used to be a browser `fetch('/plugin-ui/<id>/__health')`, which worked only because plugin UIs
// were same-origin with the console — the very arrangement 2026-08-05 review H-3 removed. From a
// separate origin the browser could not read the answer without us serving CORS, so the probe moved
// here, to the console's own origin, and is done server-side.
//
// Session-gated like every other console route (it is not under a public prefix), so an
// unauthenticated LAN peer cannot enumerate which plugins are running.
import { defineEventHandler, getRouterParam, setResponseStatus } from "h3";
import { mgmtFetch } from "../../util/forward";
import {
	fetchUiCredential,
	hostRelayPath,
	PLUGIN_ID_RE,
	viaHost,
} from "../../util/pluginProxy";

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id") ?? "";
	if (!PLUGIN_ID_RE.test(id)) {
		setResponseStatus(event, 400);
		return { ok: false, error: "not a valid plugin id" };
	}
	const cred = await fetchUiCredential(id);
	if (!cred) {
		setResponseStatus(event, 502);
		return { ok: false, error: `plugin "${id}" is not running` };
	}
	try {
		// The plugin's UI server is loopback-only and plain HTTP, exactly as the proxy dials it;
		// a plugin with no listener is asked through the host.
		const resp = viaHost(cred)
			? await mgmtFetch(hostRelayPath(id, "/__health"), { redirect: "manual" })
			: await fetch(`http://127.0.0.1:${cred.port}/__health`, {
					headers: { authorization: `Bearer ${cred.secret}` },
					redirect: "manual",
				});
		if (!resp.ok) {
			setResponseStatus(event, 502);
			return { ok: false, error: `health ${resp.status}` };
		}
		return { ok: true };
	} catch {
		// Port died between the credential lookup and the probe (plugin restarting).
		setResponseStatus(event, 502);
		return { ok: false, error: `plugin "${id}" is not reachable` };
	}
});
