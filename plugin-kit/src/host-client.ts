// The pf seam as Effect services. `HostClient.request` keeps the SDK's deliberately
// untyped `pf.request` wire (version-skew-safe against the runner-bundled SDK —
// plugin-ui-surface.md D7); typing happens one level up (wire.ts owns the wire schemas).
// Only the plain `pf` object crosses the plugin boundary, so no Effect values are shared
// with the runner's bundled effect instance.
import type { Punktfunk } from "@punktfunk/host";
import { Context, Effect, Layer } from "effect";
import { HostRequestError } from "./errors.js";

export interface HostClientService {
	/** Raw management-API call (loopback + bearer, via the SDK facade). */
	readonly request: (
		method: string,
		path: string,
		body?: unknown,
	) => Effect.Effect<unknown, HostRequestError>;
	/**
	 * Escape hatch for SDK helpers that take the facade directly (`servePluginUi`).
	 * Never share this with code that could leak Effect values back to the runner.
	 */
	readonly facade: Punktfunk;
}

export class HostClient extends Context.Service<
	HostClient,
	HostClientService
>()("@punktfunk/plugin-kit/HostClient") {}

/** Wrap the facade the runner hands to `main` (or `connect()` in the CLI/dev paths). */
export const hostClientFromFacade = (pf: Punktfunk): Layer.Layer<HostClient> =>
	Layer.succeed(HostClient)({
		request: (method, path, body) =>
			Effect.tryPromise({
				try: () => pf.request(method, path, body),
				catch: (cause) => new HostRequestError({ method, path, cause }),
			}),
		facade: pf,
	});

export interface PluginInfoService {
	/** The plugin id (`definePlugin` name, `[a-z][a-z0-9-]*`). */
	readonly name: string;
	readonly version?: string;
}

export class PluginInfo extends Context.Service<
	PluginInfo,
	PluginInfoService
>()("@punktfunk/plugin-kit/PluginInfo") {}

export const pluginInfoLayer = (
	info: PluginInfoService,
): Layer.Layer<PluginInfo> => Layer.succeed(PluginInfo)(info);
