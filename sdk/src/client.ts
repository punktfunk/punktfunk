// The Effect-native surface (RFC §7): the `PunktfunkHost` service — a typed management-API
// client plus the lifecycle-event `Stream` — provided by [`PunktfunkHostLive`]. Wire shapes
// are the generated Effect Schemas in ./gen/punktfunk.ts, events included; `get` validates a
// response against its schema, so host/SDK version skew surfaces as a typed [`VersionSkew`]
// instead of an `undefined` three frames later.
import {
	Context,
	Data,
	Effect,
	Layer,
	Schema as S,
	Stream,
} from "effect";
import type { Connection } from "./connection.js";
import { HttpStatusError, httpRequest } from "./http.js";
import {
	classifyFrame,
	type EventStreamOptions,
	type SseFrame,
	SseAuthError,
	sseFrames,
} from "./sse.js";
import type { HostEvent } from "./wire.js";

/** Bad credentials — the token (or paired cert) was rejected. */
export class AuthError extends Data.TaggedError("AuthError")<{
	message: string;
}> {}
/** The host answered with a non-2xx (the message is its `ApiError` envelope). */
export class ApiError extends Data.TaggedError("ApiError")<{
	status: number;
	message: string;
}> {}
/** The request never completed (connection refused, TLS, abort). */
export class TransportError extends Data.TaggedError("TransportError")<{
	cause: unknown;
}> {}
/** A 2xx body did not match its schema — host and SDK disagree on the wire shape. */
export class VersionSkew extends Data.TaggedError("VersionSkew")<{
	path: string;
	issue: string;
}> {}
/** The event stream failed unrecoverably (auth) — transient trouble self-heals via reconnect. */
export class EventStreamError extends Data.TaggedError("EventStreamError")<{
	cause: unknown;
}> {}

export type RequestError = AuthError | ApiError | TransportError;

export interface PunktfunkHostService {
	readonly config: Connection;
	/** One management-API request under `/api/v1`; the parsed JSON body. */
	readonly request: (
		method: string,
		path: string,
		body?: unknown,
	) => Effect.Effect<unknown, RequestError>;
	/** GET + schema-validate (the generated schemas from `@punktfunk/host/effect`'s `api`). */
	readonly get: <A, I>(
		path: string,
		schema: S.Codec<A, I>,
	) => Effect.Effect<A, RequestError | VersionSkew>;
	/**
	 * The lifecycle-event stream: decoded [`HostEvent`]s with automatic reconnect +
	 * `Last-Event-ID` resume. Unknown kinds and the `dropped` / `live` markers surface on
	 * [`eventsRaw`] (and, for `dropped` and unknown kinds, the warning callback), never as a
	 * failure here.
	 */
	readonly events: (
		opts?: EventStreamOptions,
	) => Stream.Stream<HostEvent, EventStreamError>;
	/** Every SSE frame verbatim — the `dropped` / `live` markers and unknown kinds included. */
	readonly eventsRaw: (
		opts?: EventStreamOptions,
	) => Stream.Stream<SseFrame, EventStreamError>;
}

export class PunktfunkHost extends Context.Service<
	PunktfunkHost,
	PunktfunkHostService
>()("@punktfunk/host/PunktfunkHost") {}

const toRequestError = (path: string, cause: unknown): RequestError => {
	if (cause instanceof HttpStatusError) {
		return cause.status === 401
			? new AuthError({ message: cause.message })
			: new ApiError({ status: cause.status, message: cause.message });
	}
	return new TransportError({ cause });
};

export const makeService = (cfg: Connection): PunktfunkHostService => {
	const request = (method: string, path: string, body?: unknown) =>
		Effect.tryPromise({
			try: () => httpRequest(cfg, method, path, body),
			catch: (cause) => toRequestError(path, cause),
		});
	const get = <A, I>(path: string, schema: S.Codec<A, I>) =>
		request("GET", path).pipe(
			Effect.flatMap((body) =>
				S.decodeUnknownEffect(schema)(body).pipe(
					Effect.mapError(
						(e) => new VersionSkew({ path, issue: String(e) }),
					),
				),
			),
		);
	const eventsRaw = (opts?: EventStreamOptions) =>
		// suspend: each run must get a FRESH generator (a generator is single-use).
		Stream.suspend(() =>
			Stream.fromAsyncIterable(
				sseFrames(cfg, opts),
				(cause) => new EventStreamError({ cause }),
			),
		);
	const events = (opts?: EventStreamOptions) => {
		const warn =
			opts?.onWarning ?? ((m: string) => console.warn(`[punktfunk] ${m}`));
		// Drop-or-emit per frame, with the `warn` side effects in the drop arms. `Stream.filterMap`
		// takes a `Filter`, so this flat-maps to `Stream.empty` (drop) / `Stream.succeed` (emit).
		return eventsRaw(opts).pipe(
			Stream.flatMap((frame) => {
				const c = classifyFrame(frame);
				switch (c.tag) {
					case "event":
						return Stream.succeed(c.event);
					case "dropped":
						warn(
							"event cursor fell off the host's ring — resync via the REST snapshots",
						);
						break;
					case "garbled":
						warn(`unparseable event frame (${frame.event})`);
						break;
					// An unknown kind from a NEWER host is expected (additive-only wire) — it
					// rides the raw channel; a consumer that wants it uses eventsRaw.
					case "unknown":
						warn(`unknown/undecodable event kind "${frame.event}"`);
						break;
				}
				// `live` ends the host's catch-up; not worth a warning per connect.
				return Stream.empty;
			}),
		);
	};
	return { config: cfg, request, get, events, eventsRaw };
};

/**
 * The service over a connection the caller already built — a browser's device key, a test's
 * stub, anything. The Node-resolving [`layer`] lives beside `config.ts`, because resolving a
 * connection from files is the one part of this that is not platform-neutral.
 */
export const layerFrom = (conn: Connection): Layer.Layer<PunktfunkHost> =>
	Layer.succeed(PunktfunkHost, makeService(conn));

export { SseAuthError };
