// SSE support. `effect/http-api` has no event-stream media type (verified at
// 4.0.1), so the status feed is a raw HttpRouter route beside the HttpApi contract —
// same wire shape the first-generation plugins used (`event: <name>` frames + comment
// pings), which is already proven through the console's reverse proxy.
import { Effect, type Layer, Schedule, Stream } from "effect";
import { HttpRouter, HttpServerResponse } from "effect/http";

const encoder = new TextEncoder();

export interface SseRouteOptions<A> {
	/** SSE `event:` name (default "message"). */
	readonly event?: string;
	/** Serialize one item (default JSON.stringify). */
	readonly encode?: (a: A) => string;
	/**
	 * Keepalive comment cadence in seconds (0 disables). Default 5 — deliberately below
	 * Bun's 10 s default `idleTimeout`, which `servePluginUi`'s server does not raise:
	 * a slower ping never arrives, because the idle connection is closed first.
	 */
	readonly pingSeconds?: number;
}

/**
 * Register `GET <path>` streaming `stream` as server-sent events. Each subscriber gets
 * an independent subscription; disconnects tear the stream down via scope closure.
 */
export const sseRoute = <A>(
	path: `/${string}`,
	stream: Stream.Stream<A>,
	opts?: SseRouteOptions<A>,
): Layer.Layer<never, never, HttpRouter.HttpRouter> => {
	const event = opts?.event ?? "message";
	const encode = opts?.encode ?? ((a: A) => JSON.stringify(a));
	const pingSeconds = opts?.pingSeconds ?? 5;

	const frames = stream.pipe(
		Stream.map((a) => `event: ${event}\ndata: ${encode(a)}\n\n`),
	);
	const pings =
		pingSeconds > 0
			? Stream.fromSchedule(Schedule.spaced(`${pingSeconds} seconds`)).pipe(
					Stream.map(() => `: ping\n\n`),
				)
			: Stream.empty;

	const body = Stream.merge(frames, pings).pipe(
		Stream.map((s) => encoder.encode(s)),
	);

	return HttpRouter.add(
		"GET",
		path,
		Effect.succeed(
			HttpServerResponse.stream(body, {
				contentType: "text/event-stream",
				headers: {
					"cache-control": "no-cache",
					connection: "keep-alive",
					"x-accel-buffering": "no",
				},
			}),
		),
	);
};
