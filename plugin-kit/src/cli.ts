// Minimal plugin CLI scaffold. Deliberately NOT `effect/cli`: its runner needs
// Stdio/Terminal/FileSystem service implementations that only ship in platform packages,
// which would add a runtime dependency to every plugin for what is a five-verb ops tool.
// A plugin CLI is `<bin> <command> [args...]` — this dispatcher gives that shape the same
// ManagedRuntime + layer graph as the plugin entry, so commands reuse the exact services.
import { connect, type Punktfunk } from "@punktfunk/host";
import { Effect, Layer, ManagedRuntime } from "effect";
import { HostRequestError } from "./errors.js";
import {
	type HostClient,
	hostClientFromFacade,
	type PluginInfo,
	pluginInfoLayer,
} from "./host-client.js";
import { loggingLayer } from "./logging.js";
import type { PluginKitDef } from "./runtime.js";

export interface CliCommand<R> {
	readonly summary: string;
	/** Set when the command works without a running host (scan/preview style). */
	readonly offline?: boolean;
	readonly run: (
		argv: ReadonlyArray<string>,
	) => Effect.Effect<void, unknown, R | HostClient | PluginInfo>;
}

/** A HostClient whose calls fail — the offline lane for host-free commands. */
const offlineFacade = (name: string): Punktfunk =>
	({
		request: async (method: string, path: string) => {
			throw new Error(
				`${name}: this command ran offline but tried ${method} ${path} — is the host running?`,
			);
		},
		close: () => {},
	}) as unknown as Punktfunk;

const usage = <R>(
	def: { name: string; version?: string },
	commands: Record<string, CliCommand<R>>,
): string => {
	const rows = Object.entries(commands)
		.map(([cmd, c]) => `  ${cmd.padEnd(12)} ${c.summary}`)
		.join("\n");
	return `${def.name}${def.version ? ` ${def.version}` : ""}\n\nUsage: punktfunk-plugin-${def.name} <command> [args...]\n\nCommands:\n${rows}\n`;
};

/**
 * Run one CLI invocation: dispatch `process.argv[2]`, build the plugin's layer graph,
 * run the command, tear down. Exits the process (0 ok / 1 failure / 2 usage).
 */
export const runPluginCli = async <E, R>(opts: {
	readonly def: PluginKitDef<E, R>;
	readonly commands: Record<string, CliCommand<R>>;
	readonly argv?: ReadonlyArray<string>;
}): Promise<void> => {
	const argv = opts.argv ?? process.argv.slice(2);
	const [name, ...rest] = argv;
	const command = name ? opts.commands[name] : undefined;
	if (!command) {
		console.log(usage(opts.def, opts.commands));
		process.exit(name === undefined || name === "help" ? 0 : 2);
	}

	const pf = command.offline ? offlineFacade(opts.def.name) : await connect();
	const base = Layer.mergeAll(
		hostClientFromFacade(pf),
		pluginInfoLayer({ name: opts.def.name, version: opts.def.version }),
		loggingLayer(opts.def.name),
	);
	const rt = ManagedRuntime.make(Layer.provideMerge(opts.def.layer, base));
	try {
		await rt.runPromise(Effect.scoped(command.run(rest)));
		// Do NOT clobber a non-zero code the command set deliberately. `parity --compare` reports a
		// mismatch by setting `process.exitCode = 1` and then RETURNING normally — a red parity is a
		// finished comparison, not a crashed command. Assigning 0 here unconditionally overwrote it,
		// so the one verb documented as a release gate ("exits non-zero on any difference", "do not
		// publish a version whose parity run is red") always exited 0, and any scripted use of it
		// passed. MEASURED against a live host on 2026-08-06: `parity FAILED — 1 missing`, exit 0.
		process.exitCode ??= 0;
	} catch (e) {
		const hint =
			e instanceof HostRequestError ? " (is the Punktfunk host running?)" : "";
		console.error(`${opts.def.name}: ${name} failed: ${e}${hint}`);
		process.exitCode = 1;
	} finally {
		await rt.dispose();
		pf.close();
	}
};
