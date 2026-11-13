// /api/v1/profiles/<id>/proxy/** — the console reaching a seat profile's own host for its library,
// game sources and plugins. Wins over the `/api/**` catch-all by h3 route specificity, which is
// the only place the box's password gates and its credential denial can see this path.
//
// A call a gate guards on the box's own host goes through that gate's handler, which forwards to
// the seat's path (`event.context.seat`, read by `forwardJson`). A stream the box serves through
// `proxySse` goes through it here too. Everything else relays as the catch-all does. All read the
// path as `seatCall` resolved it.
import { createError, defineEventHandler, getRequestURL } from "h3";
import { seatGate } from "../../../../../../util/gates";
import { relayToMgmt } from "../../../../../../util/mgmt";
import {
	encodeRest,
	isPluginPrivate,
	seatCall,
	seatPath,
} from "../../../../../../util/seatProxy";
import { proxySse } from "../../../../../../util/sseProxy";

export default defineEventHandler((event) => {
	const { pathname, search } = getRequestURL(event);
	const call = seatCall(pathname);
	if (!call) {
		throw createError({ statusCode: 404, statusMessage: "no such seat path" });
	}
	if (isPluginPrivate(call.rest)) {
		throw createError({
			statusCode: 403,
			statusMessage:
				"plugin UI credentials and pages are not accessible from the browser",
		});
	}
	event.context.seat = call.id;
	const gate = seatGate(event.method, call.rest);
	if (gate) {
		event.context.params = { ...event.context.params, ...gate.params };
		return gate.handler(event);
	}
	// The relay holds every write until the upstream ends, which a stream never does.
	if (event.method === "GET" && /^(events|session\/\d+\/pads)$/.test(call.rest))
		return proxySse(
			event,
			`profiles/${call.id}/proxy/${encodeRest(call.rest)}`,
		);
	return relayToMgmt(
		event,
		`${seatPath(call.id, encodeRest(call.rest))}${search}`,
	);
});
