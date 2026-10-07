// /api/v1/profiles/<id>/proxy/** — the console reaching a seat profile's own host for its library,
// game sources and plugins. Wins over the `/api/**` catch-all by h3 route specificity, which is
// the only place the box's password gates and its credential denial can see this path.
//
// A call a gate guards on the box's own host goes through that gate's handler, which forwards to
// the seat's path (`event.context.seat`, read by `forwardJson`). Everything else relays as the
// catch-all does. Both read the path as `seatCall` resolved it.
import {
	createError,
	defineEventHandler,
	getRequestURL,
	proxyRequest,
} from "h3";
import { loopbackTls, mgmtUrl } from "../../../../../../util/auth";
import {
	assertHostTokenAccepted,
	mgmtBearer,
} from "../../../../../../util/forward";
import {
	encodeRest,
	type GatedRoute,
	gatedRoute,
	isUiCredential,
	seatCall,
	seatPath,
} from "../../../../../../util/seatProxy";
import hooksPut from "../../../hooks.put";
import customPut from "../../../library/custom/[id].put";
import customPost from "../../../library/custom.post";
import providerPut from "../../../library/provider/[provider].put";
import installPost from "../../../store/install.post";
import sourcesPut from "../../../store/sources/[name].put";

const GATES: Record<GatedRoute["route"], typeof hooksPut> = {
	"hooks.put": hooksPut,
	"store/install.post": installPost,
	"store/sources/[name].put": sourcesPut,
	"library/custom.post": customPost,
	"library/custom/[id].put": customPut,
	"library/provider/[provider].put": providerPut,
};

export default defineEventHandler((event) => {
	const { pathname, search } = getRequestURL(event);
	const call = seatCall(pathname);
	if (!call) {
		throw createError({ statusCode: 404, statusMessage: "no such seat path" });
	}
	if (isUiCredential(call.rest)) {
		throw createError({
			statusCode: 403,
			statusMessage:
				"plugin UI credentials are not accessible from the browser",
		});
	}
	event.context.seat = call.id;
	const gate = gatedRoute(event.method, call.rest);
	if (gate) {
		event.context.params = { ...event.context.params, ...gate.params };
		return GATES[gate.route](event);
	}
	const base = mgmtUrl();
	return proxyRequest(
		event,
		`${base}${seatPath(call.id, encodeRest(call.rest))}${search}`,
		{
			fetchOptions: loopbackTls(base) as unknown as RequestInit | undefined,
			headers: { authorization: mgmtBearer(), cookie: "" },
			onResponse: (_event, response) => assertHostTokenAccepted(response),
		},
	);
});
