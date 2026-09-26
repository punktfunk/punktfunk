// `@punktfunk/host` — the Promise facade, the package's front door (RFC §7): `connect()`,
// `await`, `.on()` — a thin veneer over the same core the Effect surface uses (config
// resolution, one HTTP implementation, one reconnecting SSE source). Effect is the substrate
// and the power surface (`@punktfunk/host/effect`), never a prerequisite:
//
//   import { connect } from "@punktfunk/host";
//
//   const pf = await connect();
//   pf.events.on("pairing.pending", async (e) => {
//     await notifyPhone(`Pairing request from ${e.device.name}`);
//   });
import { type Effect, Result } from "effect";
import { type HostApi, makeHostApi } from "./api.js";
import type { PunktfunkHost } from "./client.js";
import {
	type ConnectOptions,
	type ResolvedConfig,
	resolveConfig,
} from "./config.js";
import { HttpStatusError, httpRequest } from "./http.js";
import { type SseFrame, sseFrames } from "./sse.js";
import {
	decodeHostEvent,
	type EventOf,
	type HostEvent,
	type HostEventKind,
	kindMatches,
} from "./wire.js";

export type { HostApi } from "./api.js";
export { HttpStatusError } from "./http.js";
export type { ConnectOptions } from "./config.js";
// A plugin persists its state here — the one dir the de-privileged Windows runner may write.
export { pluginStateDir } from "./config.js";
// A plugin reads cross-account data (dropped by an interactive-user app) from here.
export { pluginIngestDir } from "./config.js";
export {
	type PluginUiHandle,
	type PluginUiOptions,
	servePluginUi,
} from "./ui.js";
export type {
	ClientRef,
	DeviceRef,
	DisconnectReason,
	EventOf,
	HostEvent,
	HostEventKind,
	Plane,
	PresetRef,
	SessionRef,
	StreamRef,
} from "./wire.js";

/** `on()` also accepts these beyond the typed kinds. */
type SpecialPattern = "*" | "dropped" | "unknown" | (string & {});

interface Listener {
	pattern: string;
	cb: (ev: never) => unknown;
}

export interface PunktfunkEvents {
	/**
	 * Subscribe to lifecycle events. `pattern` is an exact kind (`"stream.started"` — the
	 * callback is typed to it), a `domain.*` prefix, `"*"` (every known event), `"dropped"`
	 * (the fell-off-the-ring marker), or `"unknown"` (kinds this SDK doesn't know — a newer
	 * host). Returns the unsubscribe function. Callback errors are caught and warned, never
	 * fatal to the stream.
	 */
	on<K extends HostEventKind>(
		pattern: K,
		cb: (ev: EventOf<K>) => unknown,
	): () => void;
	on(pattern: SpecialPattern, cb: (ev: HostEvent) => unknown): () => void;
}

export interface Punktfunk {
	/** Resolved connection (URL/token/CA). */
	readonly config: ResolvedConfig;
	/**
	 * The **typed** management API — every REST endpoint as an autocompletable method with
	 * checked request/response types (`await pf.api.listPairedClients()`). This is the front
	 * door; reach for [`request`] only for something the generated client doesn't cover.
	 */
	readonly api: HostApi;
	/** Untyped escape hatch: one raw `/api/v1` request (`request("GET", "/status")`). */
	request(method: string, path: string, body?: unknown): Promise<unknown>;
	/** The lifecycle-event subscription surface. */
	readonly events: PunktfunkEvents;
	/** Stop the event stream and release the connection. */
	close(): void;
}

/**
 * Connect to the host's management API: resolves the URL, bearer token, and the host's
 * self-signed identity cert from the environment/host files (zero config on the host box),
 * verifies the credentials with a `/health`-adjacent probe, and returns the client.
 */
export const connect = async (options?: ConnectOptions): Promise<Punktfunk> => {
	const cfg = await resolveConfig(options);
	// Fail fast on bad credentials/URL: one cheap authenticated probe.
	await httpRequest(cfg, "GET", "/host");
	const api = await makeHostApi(cfg);

	const listeners = new Set<Listener>();
	let pump: AsyncGenerator<SseFrame> | undefined;
	let closed = false;

	const warn = (m: string) => console.warn(`[punktfunk] ${m}`);
	const dispatch = (pattern: string, ev: unknown) => {
		for (const l of listeners) {
			if (l.pattern === pattern || (pattern !== "dropped" && pattern !== "unknown" && (l.pattern === "*" || kindMatches(l.pattern, pattern)))) {
				try {
					(l.cb as (e: unknown) => unknown)(ev);
				} catch (e) {
					warn(`listener for "${l.pattern}" threw: ${e}`);
				}
			}
		}
	};
	const startPump = () => {
		if (pump || closed) return;
		pump = sseFrames(cfg, { onWarning: warn });
		void (async () => {
			try {
				for await (const frame of pump) {
					if (closed) break;
					if (frame.event === "dropped") {
						dispatch("dropped", frame);
						continue;
					}
					// End of the host's catch-up, not an event: never an `"unknown"` dispatch.
					if (frame.event === "live") continue;
					let json: unknown;
					try {
						json = JSON.parse(frame.data);
					} catch {
						continue;
					}
					const decoded = decodeHostEvent(json);
					if (Result.isFailure(decoded)) {
						dispatch("unknown", json);
						continue;
					}
					dispatch(decoded.success.kind, decoded.success);
				}
			} catch (e) {
				if (!closed) warn(`event stream stopped: ${e}`);
			}
		})();
	};

	return {
		config: cfg,
		api,
		request: (method, path, body) => httpRequest(cfg, method, path, body),
		events: {
			on(pattern: string, cb: (ev: never) => unknown) {
				const l: Listener = { pattern, cb };
				listeners.add(l);
				startPump();
				return () => listeners.delete(l);
			},
		} as PunktfunkEvents,
		close() {
			closed = true;
			pump?.return(undefined).catch(() => {});
			pump = undefined;
		},
	};
};

// ------------------------------------------------------------------- plugins (RFC §8)

/**
 * A plugin's `main`: either a plain async function receiving the connected client, or an
 * Effect requiring the `PunktfunkHost` service (`@punktfunk/host/effect`) — the shape that
 * makes it well-behaved under the managed runner's supervision (structured interruption,
 * scoped finalizers).
 */
export type PluginMain =
	| ((pf: Punktfunk) => Promise<unknown> | unknown)
	| Effect.Effect<unknown, unknown, PunktfunkHost>;

export interface PluginDef {
	/** Package-convention name (`punktfunk-plugin-*` drops the prefix): kebab-case. */
	name: string;
	main: PluginMain;
}

/**
 * Declare a plugin (the `punktfunk-plugin-*` convention, RFC §8). In v1 a plugin is a script
 * the operator runs; the managed runner (a later, optional package) discovers this default
 * export and supervises `main`.
 */
export const definePlugin = (def: PluginDef): PluginDef => {
	if (!/^[a-z][a-z0-9-]*$/.test(def.name)) {
		throw new Error(
			`plugin name "${def.name}" must be kebab-case ([a-z][a-z0-9-]*)`,
		);
	}
	return def;
};
