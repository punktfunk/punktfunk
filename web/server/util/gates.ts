// The console's password gate, as one table: every mutating call on the management API, and
// whether a session alone may make it. A gated row's file under `routes/api/v1/` asks for the
// console password before it forwards, on every call or on those its `when` names; a refused
// row's file answers and never forwards. `seat` is that file's handler when the same call can
// arrive for a seat's host through `/api/v1/profiles/<id>/proxy/`, where the box holds the bearer.
// gates.test.ts fails on an operation in api/openapi.json with no row or no decision, and pins
// the rows to the route files and to the paths a seat's host refuses.
import type { EventHandler } from "h3";
import adoptPost from "../routes/api/v1/emulators/[id]/adopt.post";
import removePost, {
	when as purges,
} from "../routes/api/v1/emulators/[id]/remove.post";
import exportPost from "../routes/api/v1/emulators/[id]/saves/export.post";
import importPost from "../routes/api/v1/emulators/[id]/saves/import.post";
import relayPost from "../routes/api/v1/events/relay.post";
import holdReleasePost from "../routes/api/v1/game/holds/[token]/release.post";
import hooksPut from "../routes/api/v1/hooks.put";
import settingsPatch, {
	when as touchesPlane,
} from "../routes/api/v1/host/settings.patch";
import customPut from "../routes/api/v1/library/custom/[id].put";
import customPost from "../routes/api/v1/library/custom.post";
import providerPut from "../routes/api/v1/library/provider/[provider].put";
import { when as widens } from "../routes/api/v1/native/clients/[fingerprint].patch";
import decidePost, {
	when as allows,
} from "../routes/api/v1/plugin-access/[plugin]/decide.post";
import pluginPut from "../routes/api/v1/plugins/[id].put";
import installPost from "../routes/api/v1/store/install.post";
import sourcesPut from "../routes/api/v1/store/sources/[name].put";
import type { When } from "./confirm";

type Method = "POST" | "PUT" | "PATCH" | "DELETE";

export interface Route {
	method: Method;
	/** The path after `/api/v1/`, spelled as `api/openapi.json` spells it. */
	pattern: string;
	/** Why a session alone may make the call. Absent on a gated or refused row. */
	open?: string;
	/** The console answers the call itself and never forwards it. */
	refused?: true;
	/** A gated row asks only on the calls this names; absent, it asks on every call. */
	when?: When;
	/** The row's handler for a seat call. Absent when the seat's host refuses the path. */
	seat?: EventHandler;
}

const gated = (
	method: Method,
	pattern: string,
	seat?: EventHandler,
): Route => ({
	method,
	pattern,
	seat,
});

/** `route`, asking only on the calls `when` names. */
const onlyWhen = (when: When, route: Route): Route => ({ ...route, when });

const refused = (
	method: Method,
	pattern: string,
	seat: EventHandler,
): Route => ({ method, pattern, refused: true, seat });

const open = (method: Method, pattern: string, reason: string): Route => ({
	method,
	pattern,
	open: reason,
});

const NARROWS = "only narrows what the host trusts or runs";
const STEERS = "steers a session, seat or display that already exists";
const WITHIN =
	"moves a live session within its pairing's grants, never past them";
const SETTING = "a setting or a label: no command, device or identity in it";
const REPORT = "a plugin's or a device's own report: grants nothing";
const RECORD = "diagnostics: re-reads, records or drops a record";
const REFETCH = "stops or removes a download: saves stay";
const REFRESH = "re-reads a source the host already trusts";
const PUBLIC = "open on the host too: a device proving its pairing";
const VERIFIED = "fetched from its own release channel and verified";
const RUNS_NONE = "hands files to an installed emulator; runs no new program";
const LAUNCHES = "what every launch of that emulator already does";
const DOWNLOADS = "an installed plugin downloads its own title";
const INSTALLED =
	"starts only installed plugins: each install was the decision";
const ADMITS_NONE =
	"makes a seat but admits no device; removing one is gated. A Windows password it carries is one the caller already holds";
const RESEALS =
	"a setting, a label, or a Windows password the caller already holds: no command, device or identity in it";

export const GATES: readonly Route[] = [
	gated("POST", "actions/{id}"),
	open("POST", "auth/device/challenge", PUBLIC),
	open("POST", "auth/device/token", PUBLIC),
	open("POST", "client-logs", REPORT),
	open("DELETE", "client-logs/{id}", RECORD),
	open("DELETE", "clients", NARROWS),
	open("PATCH", "clients/{fingerprint}", SETTING),
	open("DELETE", "clients/{fingerprint}", NARROWS),
	open("POST", "diagnostics/refresh", RECORD),
	open("PUT", "display/clients/{fingerprint}", SETTING),
	open("DELETE", "display/clients/{fingerprint}", SETTING),
	open("PUT", "display/layout", SETTING),
	open("POST", "display/presets", SETTING),
	open("PUT", "display/presets/{id}", SETTING),
	open("DELETE", "display/presets/{id}", SETTING),
	open("POST", "display/release", STEERS),
	open("PUT", "display/settings", SETTING),
	gated("POST", "emulators/{id}/adopt", adoptPost),
	open("POST", "emulators/{id}/content", RUNS_NONE),
	open("POST", "emulators/{id}/install", VERIFIED),
	open("POST", "emulators/{id}/prepare", LAUNCHES),
	onlyWhen(purges, gated("POST", "emulators/{id}/remove", removePost)),
	gated("POST", "emulators/{id}/saves/export", exportPost),
	gated("POST", "emulators/{id}/saves/import", importPost),
	open("POST", "game/end", STEERS),
	refused("POST", "game/holds/{token}/release", holdReleasePost),
	refused("POST", "events/relay", relayPost),
	open("PUT", "gpus/preference", SETTING),
	gated("PUT", "hooks", hooksPut),
	onlyWhen(touchesPlane, gated("PATCH", "host/settings", settingsPatch)),
	gated("POST", "library/custom", customPost),
	gated("PUT", "library/custom/{id}", customPut),
	open("DELETE", "library/custom/{id}", NARROWS),
	open("PUT", "library/hidden/{id}", SETTING),
	open("POST", "library/install/{id}", DOWNLOADS),
	open("DELETE", "library/install/{id}", REFETCH),
	open("POST", "library/install/{id}/cancel", REFETCH),
	open("POST", "library/install/{id}/pause", REFETCH),
	open("PUT", "library/metadata", SETTING),
	open("PUT", "library/metadata/{source}", REPORT),
	open("DELETE", "library/metadata/{source}", NARROWS),
	open("PUT", "library/picks/{id}", SETTING),
	gated("PUT", "library/provider/{provider}", providerPut),
	open("DELETE", "library/provider/{provider}", NARROWS),
	open("PUT", "library/provider/{provider}/downloads", REPORT),
	open("PUT", "library/provider/{provider}/running", REPORT),
	open("PUT", "library/scanners/{id}", SETTING),
	open("DELETE", "native/clients", NARROWS),
	onlyWhen(widens, gated("PATCH", "native/clients/{fingerprint}")),
	open("DELETE", "native/clients/{fingerprint}", NARROWS),
	open("DELETE", "native/pair", NARROWS),
	gated("POST", "native/pair/arm"),
	gated("POST", "native/pending/{id}/approve"),
	open("POST", "native/pending/{id}/deny", NARROWS),
	gated("POST", "pair/pin"),
	open("POST", "plugin-access/requests", REPORT),
	onlyWhen(allows, gated("POST", "plugin-access/{plugin}/decide", decidePost)),
	open("POST", "plugin-access/{plugin}/release", NARROWS),
	open("POST", "plugins/logs", REPORT),
	refused("PUT", "plugins/{id}", pluginPut),
	open("DELETE", "plugins/{id}", NARROWS),
	open("POST", "profiles", ADMITS_NONE),
	open("PUT", "profiles/default", SETTING),
	gated("PUT", "profiles/door"),
	gated("PUT", "profiles/seating"),
	open("PUT", "profiles/{id}", RESEALS),
	gated("DELETE", "profiles/{id}"),
	open("PUT", "profiles/{id}/avatar", SETTING),
	open("DELETE", "profiles/{id}/avatar", SETTING),
	open("POST", "profiles/{id}/end", STEERS),
	open("POST", "profiles/{id}/start", STEERS),
	open("POST", "profiles/{id}/stop", STEERS),
	open("POST", "profiles/{id}/wake", STEERS),
	open("DELETE", "session", STEERS),
	open("POST", "session/idr", STEERS),
	open("PUT", "session/settings", SETTING),
	open("DELETE", "session/{id}", STEERS),
	open("PUT", "session/{id}/access", WITHIN),
	open("PUT", "session/{id}/audio", STEERS),
	open("POST", "session/{id}/idr", STEERS),
	open("PUT", "session/{id}/player", STEERS),
	open("POST", "stats/capture/start", RECORD),
	open("POST", "stats/capture/stop", RECORD),
	open("DELETE", "stats/recordings/{id}", RECORD),
	gated("POST", "store/install", installPost),
	open("POST", "store/refresh", REFRESH),
	open("POST", "store/runtime", INSTALLED),
	gated("PUT", "store/sources/{name}", sourcesPut),
	open("DELETE", "store/sources/{name}", NARROWS),
	open("POST", "store/uninstall", NARROWS),
	gated("POST", "update/apply"),
	open("POST", "update/check", REFRESH),
];

/** The file under `routes/api/v1/` that serves a row, without its `.ts`. */
export function routeFile(route: Route): string {
	const path = route.pattern.replace(/\{(\w+)\}/g, "[$1]");
	return `${path}.${route.method.toLowerCase()}`;
}

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

export interface SeatGate {
	handler: EventHandler;
	/** The route params the handler reads. */
	params: Record<string, string>;
}

/** The box's own password gate that guards `method` `rest` on a seat's host, if any. */
export function seatGate(method: string, rest: string): SeatGate | null {
	const verb = method.toUpperCase();
	for (const route of GATES) {
		if (!route.seat || route.method !== verb) continue;
		const params = match(route.pattern, rest);
		if (params) return { handler: route.seat, params };
	}
	return null;
}
