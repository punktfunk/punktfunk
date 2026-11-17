// Proxy one host SSE stream with the body left STREAMING.
//
// Why these routes exist at all, when the `/api/**` catch-all already proxies everything:
// the generic path cannot stream. h3's `proxyRequest` pumps the upstream body into the node-style
// response with `res.write`, and under the deployed Bun entry that response is a `node-mock-http`
// object whose writes are accumulated and only turned into a real Response when the handler
// returns. Frames sent one second apart arrive at the browser together, when the upstream
// closes. For an SSE stream that is fatal — it never closes, so nothing ever arrives, and every
// event-driven update in the console would silently never fire.
//
// Returning a WEB `Response` whose body is the upstream's own `ReadableStream` sidesteps the
// node-response emulation entirely: h3 hands it back as-is and the Bun entry passes it through.
//
// Everything else matches the catch-all: session-gated by middleware/auth.ts, and the call itself
// goes through `mgmtFetch`.
import { createError, getRequestHeader, getRequestURL, type H3Event } from "h3";
import { mgmtFetch } from "./forward";

/** `path` is the host route below `/api/v1`, e.g. `events` or `session/7/pads`. */
export async function proxySse(
	event: H3Event,
	path: string,
): Promise<Response> {
	const { search } = getRequestURL(event);
	const headers: Record<string, string> = {
		accept: "text/event-stream",
		// Ask for no compression: a buffering encoder defeats the point of a live stream.
		"accept-encoding": "identity",
	};
	// Forward the SSE resume cursor so a reconnect replays from the host's ring rather than
	// silently skipping whatever happened while we were away.
	const lastId = getRequestHeader(event, "last-event-id");
	if (lastId) headers["last-event-id"] = lastId;

	const upstream = await mgmtFetch(`/api/v1/${path}${search}`, {
		method: "GET",
		headers,
		redirect: "manual",
	});
	if (!upstream.ok || !upstream.body) {
		throw createError({
			statusCode: 502,
			statusMessage: `management API refused the event stream (${upstream.status})`,
		});
	}

	// The upstream body, untouched. `no-transform` + `X-Accel-Buffering: no` tell any intermediary
	// (and Nitro's own compression) to keep their hands off a live stream.
	return new Response(upstream.body, {
		status: 200,
		headers: {
			"content-type": "text/event-stream; charset=utf-8",
			"cache-control": "no-cache, no-transform",
			connection: "keep-alive",
			"x-accel-buffering": "no",
		},
	});
}
