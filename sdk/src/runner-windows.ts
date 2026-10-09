// Launching a plugin out of process on Windows: a process of its own, or an AppContainer through
// `punktfunk-host plugins spawn`. Both reach the host over the pipe it serves for the plugin's id.
import { Effect } from "effect";
import * as fs from "node:fs";
import * as path from "node:path";
import { configDir } from "./config.js";
import type { RunnerOptions, Unit } from "./runner.js";
import { adoptNestedState, spawnUnitChild, unitArgv } from "./runner-linux.js";
import type { LogSink } from "./runner-log.js";
import { hostExe, type PluginManifest } from "./sandbox.js";

/**
 * Run one plugin in a process of its own with nothing around it: the Windows shape with the
 * sandbox off. The child re-execs this runner in `--run-unit` mode and resolves its own token
 * and host URL from the config dir, so nothing secret rides in its environment or argv. A crash
 * is an exit code, and the supervisor restarts this plugin alone.
 */
export const runInOwnProcess = (
	unit: Unit,
	options: RunnerOptions,
): Effect.Effect<"plugin", unknown> =>
	Effect.suspend(() =>
		spawnUnitChild(
			process.execPath,
			unitArgv(unit),
			{
				...process.env,
				...(options.configDir ? { PUNKTFUNK_CONFIG_DIR: options.configDir } : {}),
				...pipeEnv(unit),
			},
			"plugin process",
		),
	);

/**
 * Run one plugin in its own AppContainer: `punktfunk-host plugins spawn` creates the profile for
 * this account and starts this runner's `--run-unit` child inside it, relaying its exit code
 * and ending the child with itself. The child holds no token: its own pipe is its credential.
 */
export const runInContainer = (
	unit: Unit,
	manifest: PluginManifest,
	options: RunnerOptions,
	log: LogSink,
): Effect.Effect<"plugin", unknown> =>
	Effect.suspend(() => {
		const id = manifest.id ?? unit.name;
		const config = options.configDir ?? configDir();
		adoptNestedState(path.join(config, "plugin-state", id), id, log);
		const pipe = pipeEnv(unit);
		return spawnUnitChild(
			hostExe(),
			["plugins", "spawn", "--package", id, "--", process.execPath, ...unitArgv(unit)],
			{
				...process.env,
				...(options.configDir ? { PUNKTFUNK_CONFIG_DIR: options.configDir } : {}),
				...pipe,
				// The container can bind nothing the console dials: its page goes over the pipe.
				...(pipe.PUNKTFUNK_MGMT_UNIX ? { PUNKTFUNK_UI_CHANNEL: id } : {}),
			},
			"sandboxed plugin",
		);
	});

/**
 * Where a Windows plugin reaches the host: the pipe the host serves for its id, plain HTTP, no
 * port. A host without the pipe leaves this empty and the child dials the port.
 */
const pipeEnv = (unit: Unit): Record<string, string> => {
	const id = unit.manifest?.id;
	if (process.platform !== "win32" || !id) return {};
	const pipe = `\\\\.\\pipe\\punktfunk-plugin-${id}`;
	if (!fs.existsSync(pipe)) return {};
	return { PUNKTFUNK_MGMT_URL: "http://punktfunk.host", PUNKTFUNK_MGMT_UNIX: pipe };
};
