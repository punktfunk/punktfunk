// The async-main boundary — the one place the two-effect-instances problem is handled.
//
// The packaged runner bundles its OWN copy of effect + the SDK; a plugin package's imports
// resolve to the plugin's node_modules. An Effect-shaped `main` would therefore hand the
// runner Effect values built by a different effect instance, possibly another 4.x release.
// The kit sidesteps this by construction: the plugin exports a plain async `main`, and
// EVERYTHING Effect happens inside a ManagedRuntime built from the plugin's own effect
// instance. Only the plain `pf` facade object crosses the boundary.
//
// Shutdown: the runner interrupts its supervision tree on SIGINT/SIGTERM, but it cannot
// cancel an in-flight promise — so the kit installs its own signal hooks, interrupts the
// plugin fiber (running scoped finalizers: UI deregistration, watcher close, reporter close)
// and bounds the whole teardown with `shutdownGraceMs` so `main` always resolves.
import {
	connect,
	definePlugin,
	type PluginDef,
	type Punktfunk,
} from "@punktfunk/host";
import {
	Cause,
	Effect,
	Exit,
	Fiber,
	Layer,
	ManagedRuntime,
	type Scope,
} from "effect";
import {
	type HostClient,
	hostClientFromFacade,
	type PluginInfo,
	pluginInfoLayer,
} from "./host-client.js";
import { loggingLayer } from "./logging.js";

export interface PluginKitDef<E, R> {
	/** Plugin id (`[a-z][a-z0-9-]*`) — also the registry id and provider id. */
	readonly name: string;
	readonly version?: string;
	/** The plugin's service graph, built over the kit base (HostClient | PluginInfo). */
	readonly layer: Layer.Layer<R, E, HostClient | PluginInfo>;
	/** The long-running program. Scoped: acquired resources release on interruption. */
	readonly main: Effect.Effect<
		void,
		E,
		R | HostClient | PluginInfo | Scope.Scope
	>;
	/** Upper bound on graceful teardown after a signal (default 5000 ms). */
	readonly shutdownGraceMs?: number;
}

const sleep = (ms: number): Promise<"timeout"> =>
	new Promise((resolve) => {
		const t = setTimeout(() => resolve("timeout"), ms);
		(t as { unref?: () => void }).unref?.();
	});

const runWithFacade = async <E, R>(
	def: PluginKitDef<E, R>,
	pf: Punktfunk,
): Promise<void> => {
	const base = Layer.mergeAll(
		hostClientFromFacade(pf),
		pluginInfoLayer({ name: def.name, version: def.version }),
		loggingLayer(def.name),
	);
	const rt = ManagedRuntime.make(Layer.provideMerge(def.layer, base));
	const graceMs = def.shutdownGraceMs ?? 5000;

	const fiber = rt.runFork(Effect.scoped(def.main));

	// Fires graceMs after the first signal; never before a signal — so racing against it
	// is a no-op in normal operation and a hard teardown bound once a stop is requested.
	let fireGrace: (v: "timeout") => void = () => {};
	const gracePromise = new Promise<"timeout">((resolve) => {
		fireGrace = resolve;
	});
	let stopping = false;
	const onSignal = () => {
		if (stopping) return;
		stopping = true;
		void sleep(graceMs).then(fireGrace);
		rt.runFork(Fiber.interrupt(fiber));
	};
	process.on("SIGINT", onSignal);
	process.on("SIGTERM", onSignal);

	try {
		const joined = rt.runPromise(Effect.exit(Fiber.join(fiber)));
		const exit = await Promise.race([joined, gracePromise]);
		if (exit === "timeout") {
			console.error(
				`[${def.name}] shutdown exceeded ${graceMs}ms — abandoning teardown`,
			);
			return;
		}
		if (Exit.isFailure(exit)) {
			const cause = exit.cause;
			// Pure interruption (signal-driven) is a clean stop; real failures propagate so
			// the runner records the crash and restarts with backoff.
			if (Cause.hasFails(cause) || Cause.hasDies(cause)) {
				throw new Error(Cause.pretty(cause));
			}
		}
	} finally {
		process.removeListener("SIGINT", onSignal);
		process.removeListener("SIGTERM", onSignal);
		await Promise.race([rt.dispose(), sleep(2000)]);
	}
};

/**
 * Build the plugin's `PluginDef` (the default export the runner discovers) from an
 * Effect-native definition. The returned def has a plain async `main`, so the runner's
 * sanity checks and supervision treat it exactly like any hand-written plugin.
 */
export const definePluginKit = <E, R>(def: PluginKitDef<E, R>): PluginDef =>
	definePlugin({
		name: def.name,
		main: (pf: Punktfunk) => runWithFacade(def, pf),
	});

/** Dev/CLI entry: `connect()` a facade ourselves and run the same program. */
export const runPluginKitDirect = async <E, R>(
	def: PluginKitDef<E, R>,
): Promise<void> => {
	const pf = await connect();
	try {
		await runWithFacade(def, pf);
	} finally {
		pf.close();
	}
};
