// The typed management API for the Promise facade (RFC §7): every REST endpoint of the host as
// an autocompletable method with checked request/response types — named exactly as in the
// OpenAPI document (`listPairedClients`, `stopSession`, `reconcileProviderEntries`, …). This is
// the typed front door; `pf.request(method, path, body)` stays as the untyped escape hatch.
//
// It's a thin, zero-drift veneer over the generated client (`./gen/punktfunk.ts`, from
// `@effect/openapi-generator`): the generated methods return Effects, so each is run to a Promise
// here. The transport is the SAME CA-pinning fetch the rest of the SDK uses (config.ts), fed to
// Effect's `HttpClient` via the overridable `FetchHttpClient.Fetch` reference — so the loopback
// pin is preserved and there is still exactly one place that knows how to reach the host.
import { Effect } from "effect";
import * as FetchHttpClient from "effect/http/FetchHttpClient";
import * as HttpClient from "effect/http/HttpClient";
import * as HttpClientRequest from "effect/http/HttpClientRequest";
import type { HttpClientResponse } from "effect/http/HttpClientResponse";
import type { Connection } from "./connection.js";
import * as gen from "./gen/punktfunk.js";

/**
 * Make a trailing argument that merely *accepts* `undefined` genuinely optional. The generated
 * methods type their options bag as `{…} | undefined` (a required param that tolerates `undefined`),
 * so without this you'd have to write `listPairedClients(undefined)`. Endpoints with a required
 * body (`{ payload }`, not `undefined`-able) are left untouched.
 */
type OptionalizeTail<A extends readonly unknown[]> = A extends readonly [
	...infer Init,
	infer Last,
]
	? undefined extends Last
		? [...Init, Last?]
		: A
	: A;

/**
 * Each generated method returns an Effect; expose it as a Promise of the decoded success value.
 * A call without `config` returns the bare value, so the `[value, response]` arm that the
 * generated signature adds for `includeResponse` is dropped.
 */
type Promiseify<T> = T extends (
	...args: infer A
) => Effect.Effect<infer Success, infer _E, infer _R>
	? (
			...args: OptionalizeTail<A>
		) => Promise<Exclude<Success, readonly [unknown, HttpClientResponse]>>
	: never;

/** The event stream (an Effect `Stream`) and the void catch-all — served by `pf.events` instead. */
type NonApiMethods = "httpClient" | "streamEvents" | "streamEventsSse";

/**
 * The host's management API, fully typed: `await pf.api.listPairedClients()` gives a typed array,
 * `await pf.api.reconcileProviderEntries("romm", { payload })` checks the body at compile time —
 * no hand-written paths, no `unknown` casts. A failing call rejects with the endpoint's typed
 * error. The live-event SSE endpoint is intentionally absent — subscribe via `pf.events`.
 */
export type HostApi = {
	readonly [K in Exclude<keyof gen.Punktfunk, NonApiMethods>]: Promiseify<
		gen.Punktfunk[K]
	>;
};

/**
 * The `HttpClient` the generated client runs on: base URL, the credential, and the connection's
 * fetch. Platform-neutral — this is what `@punktfunk/host/core` hands a browser.
 *
 * The credential is attached effectfully because a device key may have to run an exchange to
 * produce one, and re-earned once on a 401 because such a token lapses. A static bearer cannot
 * be re-earned, so for it a 401 is simply the answer.
 */
export const httpClientFor = (
	cfg: Connection,
): Effect.Effect<HttpClient.HttpClient> =>
	HttpClient.HttpClient.pipe(
		Effect.map((client) =>
			client.pipe(
				HttpClient.mapRequestEffect((request) =>
					Effect.promise(() => cfg.credential.header()).pipe(
						Effect.map((auth) =>
							request.pipe(
								HttpClientRequest.prependUrl(cfg.url),
								HttpClientRequest.setHeaders(auth ? { authorization: auth } : {}),
								HttpClientRequest.acceptJson,
							),
						),
					),
				),
				HttpClient.transform((response, request) =>
					cfg.credential.kind === "device"
						? response.pipe(
								Effect.flatMap((res) => {
									if (res.status !== 401) return Effect.succeed(res);
									cfg.credential.invalidate();
									return Effect.promise(() => cfg.credential.header()).pipe(
										Effect.flatMap((auth) =>
											client.execute(
												request.pipe(
													HttpClientRequest.prependUrl(cfg.url),
													HttpClientRequest.setHeaders(auth ? { authorization: auth } : {}),
													HttpClientRequest.acceptJson,
												),
											),
										),
									);
								}),
							)
						: response,
				),
			),
		),
		Effect.provide(FetchHttpClient.layer),
		// The connection's fetch: CA-pinning on Node, plain on a browser.
		Effect.provideService(FetchHttpClient.Fetch, cfg.fetch),
	);

/** Skipped at runtime too (not just in the type): these aren't Promise-shaped API calls. */
const NON_API: ReadonlySet<string> = new Set<NonApiMethods>([
	"httpClient",
	"streamEvents",
	"streamEventsSse",
]);

/** The typed API surface over a resolved connection — each method runs its Effect to a Promise. */
export const makeHostApi = async (cfg: Connection): Promise<HostApi> => {
	const client = gen.make(
		await Effect.runPromise(httpClientFor(cfg)),
	) as unknown as Record<
		string,
		unknown
	>;
	const api: Record<string, unknown> = {};
	for (const key of Object.keys(client)) {
		if (NON_API.has(key)) continue;
		const method = client[key];
		if (typeof method !== "function") continue;
		api[key] = (...args: unknown[]) =>
			Effect.runPromise(
				(method as (...a: unknown[]) => Effect.Effect<unknown>)(...args),
			);
	}
	return api as HostApi;
};
