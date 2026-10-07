// Browser/React glue for plugin UIs served through the console's /plugin-ui/<id>/ proxy.
// Design-system-free on purpose: ResultGate takes render props (the plugin wraps it once
// with its @unom/ui skeleton/error visuals), so the kit's only peer here is react.
//
// Routing model (fixes the broken deep-link restore of the first-generation UIs): the
// console pins the iframe src to the deep-linked PATH (`/plugin-ui/<id>/<route>`), so
// route init must read the last pathname segment — the hash is only a standalone-tab
// fallback. Navigation posts `pf-ui:navigate` so the console mirrors the route into its
// own URL (replace: true; the iframe src stays pinned — no reload loop).

import { Option, Schema } from "effect";
import { AsyncResult, Atom } from "effect/reactivity";
import { type ReactNode, useEffect, useState } from "react";

/** `/plugin-ui/<id>` when served through the console proxy, "" in dev/standalone. */
export const resolvePluginBase = (): string => {
	const m = window.location.pathname.match(/^\/plugin-ui\/[a-z][a-z0-9-]*/);
	return m ? m[0] : "";
};

/** True when running inside the console's iframe. */
export const useIsEmbedded = (): boolean =>
	typeof window !== "undefined" && window.parent !== window;

/**
 * Mirror a route into the console's address bar (best-effort, embedded only).
 *
 * The `"*"` target origin is load-bearing and must stay: the console frames plugin UIs from a
 * DIFFERENT ORIGIN than its own (they get their own port, so a plugin cannot act as the logged-in
 * operator — security-review 2026-08-05 H-3). Narrowing this to `window.location.origin` would
 * target the PLUGIN's origin, not the console's, and every message would be silently dropped.
 *
 * `"*"` is safe here because the payload is a route path the plugin itself just navigated to —
 * nothing secret — and the console verifies `event.origin` against the plugin origin before acting
 * on it, so the trust decision is made on the receiving side where it belongs.
 */
export const postNavigate = (path: string): void => {
	try {
		if (window.parent !== window) {
			window.parent.postMessage({ type: "pf-ui:navigate", path }, "*");
		}
	} catch {
		// detached parent — deep-link sync is best-effort
	}
};

const initialRoute = <Route extends string>(
	routes: ReadonlyArray<Route>,
	fallback: Route,
): Route => {
	const isRoute = (s: string | undefined): s is Route =>
		s !== undefined && (routes as ReadonlyArray<string>).includes(s);
	const lastSegment = window.location.pathname
		.split("/")
		.filter(Boolean)
		.at(-1);
	if (isRoute(lastSegment)) return lastSegment;
	const hashSegment = window.location.hash.replace(/^#\/?/, "");
	if (isRoute(hashSegment)) return hashSegment;
	return fallback;
};

/**
 * Flat single-segment routes, no router library. Returns a hook: route state initialized
 * from path-then-hash, `navigate` updates state + hash (standalone back/forward) + the
 * console deep-link bridge. Listens to hashchange for browser navigation.
 */
export const createPluginRouter = <const Routes extends ReadonlyArray<string>>(
	routes: Routes,
	fallback: Routes[number],
) => {
	type Route = Routes[number];
	const usePluginRoute = (): {
		route: Route;
		navigate: (r: Route) => void;
	} => {
		const [route, setRoute] = useState<Route>(() =>
			initialRoute(routes as ReadonlyArray<Route>, fallback as Route),
		);
		useEffect(() => {
			const onHash = () => {
				const seg = window.location.hash.replace(/^#\/?/, "");
				if ((routes as ReadonlyArray<string>).includes(seg)) {
					setRoute(seg as Route);
				}
			};
			window.addEventListener("hashchange", onHash);
			return () => window.removeEventListener("hashchange", onHash);
		}, []);
		const navigate = (r: Route) => {
			setRoute(r);
			window.location.hash = `/${r}`;
			postNavigate(r);
		};
		return { route, navigate };
	};
	return { routes, usePluginRoute };
};

export interface ResultGateProps<A, E> {
	readonly result: AsyncResult.AsyncResult<A, E>;
	/** Rendered while the first value loads (page skeleton). */
	readonly waiting?: ReactNode;
	/** Rendered on failure; `retry` re-triggers via the caller (registry refresh). */
	readonly failure?: (error: E, retry?: () => void) => ReactNode;
	readonly retry?: () => void;
	readonly children: (value: A) => ReactNode;
}

/**
 * The one loading/error/success convention for plugin pages. Keeps showing the last
 * value while a refresh is in flight (no skeleton flash on invalidation).
 */
export const ResultGate = <A, E>(props: ResultGateProps<A, E>): ReactNode => {
	const { result } = props;
	if (AsyncResult.isSuccess(result)) return props.children(result.value);
	if (AsyncResult.isFailure(result)) {
		const error = Option.getOrUndefined(AsyncResult.error(result)) as E;
		return props.failure?.(error, props.retry) ?? null;
	}
	// Initial or waiting-without-value.
	return props.waiting ?? null;
};

export interface SseAtomOptions<A, I> {
	/** Absolute-or-relative URL of the SSE endpoint (prefix it via resolvePluginBase). */
	readonly url: string;
	/** SSE `event:` name (default "message"). */
	readonly event?: string;
	readonly schema: Schema.Codec<A, I>;
}

/**
 * An Atom over a reconnecting EventSource: emits each schema-valid frame, `undefined`
 * until the first one. The browser reconnects EventSource automatically; the atom closes
 * it when the last subscriber unmounts.
 */
export const sseAtom = <A, I>(
	opts: SseAtomOptions<A, I>,
): Atom.Atom<A | undefined> =>
	Atom.make<A | undefined>((get) => {
		const source = new EventSource(opts.url);
		const decode = Schema.decodeUnknownSync(opts.schema);
		source.addEventListener(opts.event ?? "message", (e) => {
			try {
				get.setSelf(decode(JSON.parse((e as MessageEvent).data)));
			} catch {
				// skip schema-invalid frames (version skew tolerance)
			}
		});
		get.addFinalizer(() => source.close());
		return undefined;
	});
